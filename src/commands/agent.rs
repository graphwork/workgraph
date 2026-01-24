//! Autonomous agent runtime
//!
//! Implements the WAKE → CHECK → WORK → SLEEP cycle for autonomous agents.
//!
//! Usage:
//!   wg agent --actor <id>              # Run continuously
//!   wg agent --actor <id> --once       # Run one iteration
//!   wg agent --actor <id> --interval 30  # Custom sleep interval

use anyhow::{Context, Result};
use chrono::Utc;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::Duration;
use workgraph::graph::{LogEntry, Status, TrustLevel};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::ready_tasks;

use super::graph_path;

/// Agent run statistics
#[derive(Debug, Serialize, Default)]
pub struct AgentStats {
    pub tasks_completed: u32,
    pub tasks_failed: u32,
    pub iterations: u32,
    pub idle_iterations: u32,
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
    interval_secs: u64,
    max_tasks: Option<u32>,
    json: bool,
) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Verify actor exists
    {
        let graph = load_graph(&path).context("Failed to load graph")?;
        graph
            .get_actor(actor_id)
            .ok_or_else(|| anyhow::anyhow!("Actor '{}' not found. Register with 'wg actor add'", actor_id))?;
    }

    let mut stats = AgentStats::default();

    if !json {
        println!("🤖 Agent '{}' starting...", actor_id);
        println!("   Interval: {}s | Once: {} | Max tasks: {:?}",
                 interval_secs, once, max_tasks);
        println!();
    }

    loop {
        stats.iterations += 1;

        // WAKE: Record heartbeat
        record_heartbeat(dir, actor_id)?;

        // CHECK & WORK: Find and execute task
        let result = run_iteration(dir, actor_id, json)?;

        match result {
            IterationResult::Completed(task_id) => {
                stats.tasks_completed += 1;
                if !json {
                    println!("✓ Completed: {}", task_id);
                }
            }
            IterationResult::Failed(task_id, reason) => {
                stats.tasks_failed += 1;
                if !json {
                    println!("✗ Failed: {} - {}", task_id, reason);
                }
            }
            IterationResult::Idle => {
                stats.idle_iterations += 1;
                if !json {
                    println!("💤 No work available, sleeping {}s...", interval_secs);
                }
            }
            IterationResult::Stop(reason) => {
                if !json {
                    println!("⏹ Stopping: {}", reason);
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

    // Print final stats
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!();
        println!("Agent statistics:");
        println!("  Iterations: {}", stats.iterations);
        println!("  Tasks completed: {}", stats.tasks_completed);
        println!("  Tasks failed: {}", stats.tasks_failed);
        println!("  Idle iterations: {}", stats.idle_iterations);
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
    use workgraph::graph::{Actor, Node, Task, WorkGraph};
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

        let result = run(temp_dir.path(), "test-agent", true, 1, None, false);
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

        let result = run(temp_dir.path(), "test-agent", true, 1, None, false);
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

        // Run with max_tasks=1
        let result = run(temp_dir.path(), "test-agent", false, 1, Some(1), false);
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

        let result = run(temp_dir.path(), "test-agent", true, 1, None, false);
        assert!(result.is_ok()); // Agent should handle failures gracefully

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
    }

    #[test]
    fn test_agent_records_heartbeat() {
        let temp_dir = setup_graph();

        run(temp_dir.path(), "test-agent", true, 1, None, false).unwrap();

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

        let result = run(temp_dir.path(), "unknown-agent", true, 1, None, false);
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
}
