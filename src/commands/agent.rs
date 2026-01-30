//! Autonomous agent runtime
//!
//! Implements the WAKE -> CHECK -> WORK -> SLEEP cycle for autonomous agents.
//!
//! Usage:
//!   wg agent --actor <id>              # Run continuously
//!   wg agent --actor <id> --once       # Run one iteration
//!   wg agent --actor <id> --interval 30  # Custom sleep interval
//!   wg agent --actor <id> --reset-state # Start fresh, discarding saved state

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;
use workgraph::config::Config;
use workgraph::graph::{LogEntry, Status, TrustLevel};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::ready_tasks;

use super::graph_path;

/// Agent run statistics (session-only, not persisted)
#[derive(Debug, Serialize, Default)]
pub struct AgentStats {
    pub tasks_completed: u32,
    pub tasks_failed: u32,
    pub iterations: u32,
    pub idle_iterations: u32,
}

/// Persistent agent state - saved to .workgraph/agents/<actor-id>.json
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentState {
    /// Actor ID this state belongs to
    pub actor_id: String,

    /// Total tasks completed across all runs
    pub total_tasks_completed: u32,

    /// Total tasks failed across all runs
    pub total_tasks_failed: u32,

    /// Total iterations across all runs
    pub total_iterations: u32,

    /// Total idle iterations across all runs
    pub total_idle_iterations: u32,

    /// List of task IDs this agent has worked on (completed or failed)
    pub task_history: Vec<TaskHistoryEntry>,

    /// Number of agent sessions (incremented each startup)
    pub session_count: u32,

    /// Timestamp of first agent run
    pub first_run: Option<String>,

    /// Timestamp of last agent run
    pub last_run: Option<String>,

    /// Timestamp when state was last saved
    pub last_saved: Option<String>,
}

/// Entry in the task history
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskHistoryEntry {
    /// Task ID
    pub task_id: String,

    /// Outcome: "completed" or "failed"
    pub outcome: String,

    /// Timestamp when task was processed
    pub timestamp: String,

    /// Failure reason (if failed)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
}

impl AgentState {
    /// Create a new state for an actor
    pub fn new(actor_id: &str) -> Self {
        Self {
            actor_id: actor_id.to_string(),
            first_run: Some(Utc::now().to_rfc3339()),
            session_count: 1,
            ..Default::default()
        }
    }

    /// Get the path where this agent's state is stored
    pub fn state_path(dir: &Path, actor_id: &str) -> PathBuf {
        dir.join("agents").join(format!("{}.json", actor_id))
    }

    /// Load agent state from disk, or create new if not exists
    pub fn load(dir: &Path, actor_id: &str) -> Result<Self> {
        let path = Self::state_path(dir, actor_id);

        if !path.exists() {
            return Ok(Self::new(actor_id));
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read agent state from {:?}", path))?;

        let mut state: AgentState = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse agent state from {:?}", path))?;

        // Increment session count for this run
        state.session_count += 1;
        state.last_run = Some(Utc::now().to_rfc3339());

        Ok(state)
    }

    /// Save agent state to disk
    pub fn save(&mut self, dir: &Path) -> Result<()> {
        let agents_dir = dir.join("agents");

        // Create agents directory if it doesn't exist
        if !agents_dir.exists() {
            fs::create_dir_all(&agents_dir)
                .with_context(|| format!("Failed to create agents directory at {:?}", agents_dir))?;
        }

        let path = Self::state_path(dir, &self.actor_id);

        self.last_saved = Some(Utc::now().to_rfc3339());

        let content = serde_json::to_string_pretty(self)
            .context("Failed to serialize agent state")?;

        fs::write(&path, content)
            .with_context(|| format!("Failed to write agent state to {:?}", path))?;

        Ok(())
    }

    /// Record a completed task
    pub fn record_completed(&mut self, task_id: &str) {
        self.total_tasks_completed += 1;
        self.task_history.push(TaskHistoryEntry {
            task_id: task_id.to_string(),
            outcome: "completed".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            failure_reason: None,
        });
    }

    /// Record a failed task
    pub fn record_failed(&mut self, task_id: &str, reason: &str) {
        self.total_tasks_failed += 1;
        self.task_history.push(TaskHistoryEntry {
            task_id: task_id.to_string(),
            outcome: "failed".to_string(),
            timestamp: Utc::now().to_rfc3339(),
            failure_reason: Some(reason.to_string()),
        });
    }

    /// Delete the state file for a fresh start
    pub fn delete(dir: &Path, actor_id: &str) -> Result<()> {
        let path = Self::state_path(dir, actor_id);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to delete agent state at {:?}", path))?;
        }
        Ok(())
    }
}

/// Result of a single agent iteration
#[derive(Debug)]
enum IterationResult {
    /// Completed a task successfully
    Completed(String),
    /// Task failed
    Failed(String, String),
    /// No work available
    Idle,
    /// Agent should stop
    Stop(String),
}

/// Run the autonomous agent loop
pub fn run(
    dir: &Path,
    actor_id: &str,
    once: bool,
    interval_secs: Option<u64>,
    max_tasks: Option<u32>,
    reset_state: bool,
    json: bool,
) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Load config and apply defaults
    let config = Config::load(dir).unwrap_or_default();
    let interval_secs = interval_secs.unwrap_or(config.agent.interval);
    let max_tasks = max_tasks.or(config.agent.max_tasks);

    // Verify actor exists
    {
        let graph = load_graph(&path).context("Failed to load graph")?;
        graph
            .get_actor(actor_id)
            .ok_or_else(|| anyhow::anyhow!("Actor '{}' not found. Register with 'wg actor add'", actor_id))?;
    }

    // Handle state reset if requested
    if reset_state {
        AgentState::delete(dir, actor_id)?;
        if !json {
            println!("Agent state reset for '{}'", actor_id);
        }
    }

    // Load or create persistent state
    let mut state = AgentState::load(dir, actor_id)?;
    let mut stats = AgentStats::default();

    if !json {
        println!("Agent '{}' starting... (session #{})", actor_id, state.session_count);
        println!("   Interval: {}s | Once: {} | Max tasks: {:?}",
                 interval_secs, once, max_tasks);
        if state.total_tasks_completed > 0 || state.total_tasks_failed > 0 {
            println!("   Lifetime: {} completed, {} failed across {} sessions",
                     state.total_tasks_completed, state.total_tasks_failed, state.session_count - 1);
        }
        println!();
    }

    // Save state at startup (records last_run time)
    state.save(dir)?;

    loop {
        stats.iterations += 1;
        state.total_iterations += 1;

        // WAKE: Record heartbeat
        record_heartbeat(dir, actor_id)?;

        // CHECK & WORK: Find and execute task
        let result = run_iteration(dir, actor_id, json)?;

        match result {
            IterationResult::Completed(task_id) => {
                stats.tasks_completed += 1;
                state.record_completed(&task_id);
                state.save(dir)?; // Save after each task completion
                if !json {
                    println!("Completed: {}", task_id);
                }
            }
            IterationResult::Failed(task_id, reason) => {
                stats.tasks_failed += 1;
                state.record_failed(&task_id, &reason);
                state.save(dir)?; // Save after each task failure
                if !json {
                    println!("Failed: {} - {}", task_id, reason);
                }
            }
            IterationResult::Idle => {
                stats.idle_iterations += 1;
                state.total_idle_iterations += 1;
                if !json {
                    println!("No work available, sleeping {}s...", interval_secs);
                }
            }
            IterationResult::Stop(reason) => {
                if !json {
                    println!("Stopping: {}", reason);
                }
                break;
            }
        }

        // Check if we've hit max tasks
        if let Some(max) = max_tasks {
            if stats.tasks_completed + stats.tasks_failed >= max {
                if !json {
                    println!("Reached max tasks limit ({})", max);
                }
                break;
            }
        }

        // SLEEP: Wait before next iteration (unless --once)
        if once {
            break;
        }

        thread::sleep(Duration::from_secs(interval_secs));
    }

    // Save final state on shutdown
    state.save(dir)?;

    // Print final stats
    if json {
        // Include both session stats and lifetime stats in JSON output
        let output = serde_json::json!({
            "session": {
                "iterations": stats.iterations,
                "tasks_completed": stats.tasks_completed,
                "tasks_failed": stats.tasks_failed,
                "idle_iterations": stats.idle_iterations,
            },
            "lifetime": {
                "total_iterations": state.total_iterations,
                "total_tasks_completed": state.total_tasks_completed,
                "total_tasks_failed": state.total_tasks_failed,
                "total_idle_iterations": state.total_idle_iterations,
                "session_count": state.session_count,
                "first_run": state.first_run,
                "last_run": state.last_run,
            }
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!();
        println!("Session statistics:");
        println!("  Iterations: {}", stats.iterations);
        println!("  Tasks completed: {}", stats.tasks_completed);
        println!("  Tasks failed: {}", stats.tasks_failed);
        println!("  Idle iterations: {}", stats.idle_iterations);
        println!();
        println!("Lifetime statistics (session #{}):", state.session_count);
        println!("  Total iterations: {}", state.total_iterations);
        println!("  Total tasks completed: {}", state.total_tasks_completed);
        println!("  Total tasks failed: {}", state.total_tasks_failed);
        println!("  Total idle iterations: {}", state.total_idle_iterations);
    }

    Ok(())
}

/// Run a single iteration of the agent loop
fn run_iteration(dir: &Path, actor_id: &str, json: bool) -> Result<IterationResult> {
    let path = graph_path(dir);
    let graph = load_graph(&path).context("Failed to load graph")?;

    let actor = graph
        .get_actor(actor_id)
        .ok_or_else(|| anyhow::anyhow!("Actor '{}' not found", actor_id))?;

    let actor_skills: HashSet<&String> = actor.capabilities.iter().collect();

    // Find ready tasks
    let ready = ready_tasks(&graph);

    if ready.is_empty() {
        return Ok(IterationResult::Idle);
    }

    // Score and select best task (same logic as wg next)
    let mut best_task: Option<(&workgraph::graph::Task, i32)> = None;

    for task in &ready {
        let task_skills: HashSet<&String> = task.skills.iter().collect();
        let matched = actor_skills.intersection(&task_skills).count();
        let missing = task_skills.difference(&actor_skills).count();

        // Skip if missing required skills (unless task has no skill requirements)
        if !task_skills.is_empty() && missing > 0 && matched == 0 {
            continue;
        }

        let mut score: i32 = (matched as i32) * 10;
        score -= (missing as i32) * 5;

        if !task.skills.is_empty() && missing == 0 {
            score += 20;
        }
        if task.skills.is_empty() {
            score += 5;
        }
        if actor.trust_level == TrustLevel::Verified {
            score += 5;
        }

        // Prefer tasks with exec commands (can be automated)
        if task.exec.is_some() {
            score += 15;
        }

        if best_task.is_none() || score > best_task.unwrap().1 {
            best_task = Some((task, score));
        }
    }

    let (task, _score) = match best_task {
        Some(t) => t,
        None => return Ok(IterationResult::Idle),
    };

    let task_id = task.id.clone();
    let task_title = task.title.clone();
    let has_exec = task.exec.is_some();
    let exec_cmd = task.exec.clone();

    if !json {
        println!("→ Working on: {} - {}", task_id, task_title);
    }

    // Claim the task
    drop(graph); // Release borrow before modifying
    claim_task(dir, &task_id, actor_id)?;

    // Execute if has exec command
    if has_exec {
        let exec_cmd = exec_cmd.unwrap();
        if !json {
            println!("  Executing: {}", exec_cmd);
        }

        let output = Command::new("sh")
            .arg("-c")
            .arg(&exec_cmd)
            .output()
            .context("Failed to execute command")?;

        let success = output.status.success();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !json {
            if !stdout.is_empty() {
                for line in stdout.lines() {
                    println!("  | {}", line);
                }
            }
            if !stderr.is_empty() {
                for line in stderr.lines() {
                    eprintln!("  | {}", line);
                }
            }
        }

        if success {
            complete_task(dir, &task_id, actor_id)?;
            return Ok(IterationResult::Completed(task_id));
        } else {
            let exit_code = output.status.code().unwrap_or(-1);
            let reason = format!("Exit code {}", exit_code);
            fail_task(dir, &task_id, actor_id, &reason)?;
            return Ok(IterationResult::Failed(task_id, reason));
        }
    } else {
        // No exec command - just claim and report
        // In a real agent, this would invoke external logic
        if !json {
            println!("  No exec command - task claimed for external execution");
            println!("  Complete with: wg done {}", task_id);
        }
        // Don't auto-complete, let external process handle it
        return Ok(IterationResult::Idle);
    }
}

/// Record heartbeat for actor
fn record_heartbeat(dir: &Path, actor_id: &str) -> Result<()> {
    let path = graph_path(dir);
    let mut graph = load_graph(&path).context("Failed to load graph")?;

    if let Some(actor) = graph.get_actor_mut(actor_id) {
        actor.last_seen = Some(Utc::now().to_rfc3339());
        save_graph(&graph, &path).context("Failed to save graph")?;
    }

    Ok(())
}

/// Claim a task for the actor
fn claim_task(dir: &Path, task_id: &str, actor_id: &str) -> Result<()> {
    let path = graph_path(dir);
    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    task.status = Status::InProgress;
    task.assigned = Some(actor_id.to_string());
    task.started_at = Some(Utc::now().to_rfc3339());
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some(actor_id.to_string()),
        message: "Claimed by autonomous agent".to_string(),
    });

    save_graph(&graph, &path).context("Failed to save graph")?;
    Ok(())
}

/// Mark task as completed
fn complete_task(dir: &Path, task_id: &str, actor_id: &str) -> Result<()> {
    let path = graph_path(dir);
    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    task.status = Status::Done;
    task.completed_at = Some(Utc::now().to_rfc3339());
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some(actor_id.to_string()),
        message: "Completed by autonomous agent".to_string(),
    });

    save_graph(&graph, &path).context("Failed to save graph")?;
    Ok(())
}

/// Mark task as failed
fn fail_task(dir: &Path, task_id: &str, actor_id: &str, reason: &str) -> Result<()> {
    let path = graph_path(dir);
    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    task.status = Status::Failed;
    task.retry_count += 1;
    task.failure_reason = Some(reason.to_string());
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some(actor_id.to_string()),
        message: format!("Failed: {}", reason),
    });

    save_graph(&graph, &path).context("Failed to save graph")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Actor, ActorType, Node, Task, TrustLevel, WorkGraph};
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
            assigned: None,
            estimate: None,
            blocks: vec![],
            blocked_by: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
        }
    }

    fn make_actor(id: &str, capabilities: Vec<&str>) -> Actor {
        Actor {
            id: id.to_string(),
            name: None,
            role: None,
            rate: None,
            capacity: None,
            capabilities: capabilities.into_iter().map(String::from).collect(),
            context_limit: None,
            trust_level: TrustLevel::Provisional,
            last_seen: None,
            actor_type: ActorType::Agent,
            matrix_user_id: None,
            response_times: vec![],
        }
    }

    fn setup_graph() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Test Task");
        task.exec = Some("echo hello".to_string());

        let actor = make_actor("test-agent", vec![]);

        graph.add_node(Node::Task(task));
        graph.add_node(Node::Actor(actor));
        save_graph(&graph, &path).unwrap();

        temp_dir
    }

    #[test]
    fn test_agent_once_with_exec() {
        let temp_dir = setup_graph();

        // reset_state=true to ensure clean state for test
        let result = run(temp_dir.path(), "test-agent", true, Some(1), None, true, false);
        assert!(result.is_ok());

        // Verify task is done
        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_agent_once_no_tasks() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let actor = make_actor("test-agent", vec![]);
        graph.add_node(Node::Actor(actor));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "test-agent", true, Some(1), None, true, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_agent_max_tasks() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.exec = Some("echo 1".to_string());
        let mut t2 = make_task("t2", "Task 2");
        t2.exec = Some("echo 2".to_string());

        let actor = make_actor("test-agent", vec![]);

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Actor(actor));
        save_graph(&graph, &path).unwrap();

        // Run with max_tasks=1, reset_state=true
        let result = run(temp_dir.path(), "test-agent", false, Some(1), Some(1), true, false);
        assert!(result.is_ok());

        // Only one task should be done
        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let done_count = graph.tasks().filter(|t| t.status == Status::Done).count();
        assert_eq!(done_count, 1);
    }

    #[test]
    fn test_agent_handles_failure() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Failing Task");
        task.exec = Some("exit 1".to_string());

        let actor = make_actor("test-agent", vec![]);

        graph.add_node(Node::Task(task));
        graph.add_node(Node::Actor(actor));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "test-agent", true, Some(1), None, true, false);
        assert!(result.is_ok()); // Agent should handle failures gracefully

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
    }

    #[test]
    fn test_agent_records_heartbeat() {
        let temp_dir = setup_graph();

        run(temp_dir.path(), "test-agent", true, Some(1), None, true, false).unwrap();

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let actor = graph.get_actor("test-agent").unwrap();
        assert!(actor.last_seen.is_some());
    }

    #[test]
    fn test_agent_unknown_actor() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "unknown-agent", true, Some(1), None, true, false);
        assert!(result.is_err());
    }

    #[test]
    fn test_claim_task() {
        let temp_dir = setup_graph();

        claim_task(temp_dir.path(), "t1", "test-agent").unwrap();

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
        assert_eq!(task.assigned, Some("test-agent".to_string()));
    }

    #[test]
    fn test_complete_task() {
        let temp_dir = setup_graph();

        claim_task(temp_dir.path(), "t1", "test-agent").unwrap();
        complete_task(temp_dir.path(), "t1", "test-agent").unwrap();

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Done);
        assert!(task.completed_at.is_some());
    }

    #[test]
    fn test_fail_task() {
        let temp_dir = setup_graph();

        claim_task(temp_dir.path(), "t1", "test-agent").unwrap();
        fail_task(temp_dir.path(), "t1", "test-agent", "Test failure").unwrap();

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
        assert_eq!(task.failure_reason, Some("Test failure".to_string()));
    }

    // ===== Agent State Persistence Tests =====

    #[test]
    fn test_agent_state_new() {
        let state = AgentState::new("test-agent");
        assert_eq!(state.actor_id, "test-agent");
        assert_eq!(state.session_count, 1);
        assert!(state.first_run.is_some());
        assert_eq!(state.total_tasks_completed, 0);
        assert_eq!(state.total_tasks_failed, 0);
    }

    #[test]
    fn test_agent_state_save_and_load() {
        let temp_dir = TempDir::new().unwrap();

        // Create and save state
        let mut state = AgentState::new("test-agent");
        state.total_tasks_completed = 5;
        state.total_tasks_failed = 2;
        state.record_completed("task-1");
        state.save(temp_dir.path()).unwrap();

        // Verify state file was created
        let state_path = AgentState::state_path(temp_dir.path(), "test-agent");
        assert!(state_path.exists());

        // Load state and verify
        let loaded = AgentState::load(temp_dir.path(), "test-agent").unwrap();
        assert_eq!(loaded.actor_id, "test-agent");
        assert_eq!(loaded.total_tasks_completed, 6); // 5 + 1 from record_completed
        assert_eq!(loaded.total_tasks_failed, 2);
        assert_eq!(loaded.session_count, 2); // Incremented on load
        assert_eq!(loaded.task_history.len(), 1);
        assert_eq!(loaded.task_history[0].task_id, "task-1");
    }

    #[test]
    fn test_agent_state_load_creates_new_if_missing() {
        let temp_dir = TempDir::new().unwrap();

        let state = AgentState::load(temp_dir.path(), "new-agent").unwrap();
        assert_eq!(state.actor_id, "new-agent");
        assert_eq!(state.session_count, 1);
    }

    #[test]
    fn test_agent_state_delete() {
        let temp_dir = TempDir::new().unwrap();

        // Create and save state
        let mut state = AgentState::new("test-agent");
        state.save(temp_dir.path()).unwrap();

        // Verify it exists
        let state_path = AgentState::state_path(temp_dir.path(), "test-agent");
        assert!(state_path.exists());

        // Delete it
        AgentState::delete(temp_dir.path(), "test-agent").unwrap();
        assert!(!state_path.exists());
    }

    #[test]
    fn test_agent_state_record_completed() {
        let mut state = AgentState::new("test-agent");
        assert_eq!(state.total_tasks_completed, 0);
        assert!(state.task_history.is_empty());

        state.record_completed("task-1");
        assert_eq!(state.total_tasks_completed, 1);
        assert_eq!(state.task_history.len(), 1);
        assert_eq!(state.task_history[0].outcome, "completed");
        assert!(state.task_history[0].failure_reason.is_none());
    }

    #[test]
    fn test_agent_state_record_failed() {
        let mut state = AgentState::new("test-agent");
        assert_eq!(state.total_tasks_failed, 0);

        state.record_failed("task-1", "Exit code 1");
        assert_eq!(state.total_tasks_failed, 1);
        assert_eq!(state.task_history.len(), 1);
        assert_eq!(state.task_history[0].outcome, "failed");
        assert_eq!(state.task_history[0].failure_reason, Some("Exit code 1".to_string()));
    }

    #[test]
    fn test_agent_state_persists_across_runs() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        // Setup graph with two tasks
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("t1", "Task 1");
        t1.exec = Some("echo 1".to_string());
        let mut t2 = make_task("t2", "Task 2");
        t2.exec = Some("echo 2".to_string());
        let actor = make_actor("test-agent", vec![]);
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Actor(actor));
        save_graph(&graph, &path).unwrap();

        // First run: complete one task, don't reset state
        let result = run(temp_dir.path(), "test-agent", true, Some(1), Some(1), false, false);
        assert!(result.is_ok());

        // Load state and check
        let state = AgentState::load(temp_dir.path(), "test-agent").unwrap();
        assert_eq!(state.total_tasks_completed, 1);
        assert_eq!(state.session_count, 2); // load incremented it

        // Second run: don't reset, should accumulate (completes second task)
        let result = run(temp_dir.path(), "test-agent", true, Some(1), Some(1), false, false);
        assert!(result.is_ok());

        // Load state again (run increments on load, then saves at end)
        let state = AgentState::load(temp_dir.path(), "test-agent").unwrap();
        assert_eq!(state.total_tasks_completed, 2);
        // Session count: run1 creates session 1, run2 loads and increments to 2,
        // this load increments to 3
        assert_eq!(state.session_count, 3);
    }

    #[test]
    fn test_agent_reset_state_clears_history() {
        let temp_dir = setup_graph();

        // First run without reset
        let result = run(temp_dir.path(), "test-agent", true, Some(1), None, false, false);
        assert!(result.is_ok());

        // Verify state was saved
        let state = AgentState::load(temp_dir.path(), "test-agent").unwrap();
        assert!(state.total_tasks_completed > 0 || state.total_iterations > 0);

        // Setup another task
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = load_graph(&path).unwrap();
        let mut task = make_task("t2", "Task 2");
        task.exec = Some("echo hello".to_string());
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        // Run with reset_state=true
        let result = run(temp_dir.path(), "test-agent", true, Some(1), None, true, false);
        assert!(result.is_ok());

        // State should be reset (session_count=1 means fresh)
        let state = AgentState::load(temp_dir.path(), "test-agent").unwrap();
        // After reset + run + load, session_count should be 2
        assert_eq!(state.session_count, 2);
        // Task history should only have the new task
        assert_eq!(state.task_history.len(), 1);
        assert_eq!(state.task_history[0].task_id, "t2");
    }
}
