//! Dead agent detection and cleanup
//!
//! Detects agents that have stopped sending heartbeats and cleans them up:
//! - Marks agents as dead in the registry
//! - Unclaims their tasks (sets back to open)
//! - Optionally kills the process if still running
//!
//! Usage:
//!   wg dead-agents --check           # Just check, don't modify
//!   wg dead-agents --cleanup         # Mark dead and unclaim tasks
//!   wg dead-agents --threshold 10    # Use 10-minute threshold (default: from config)

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::config::Config;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};
use workgraph::service::{AgentRegistry, AgentStatus};

use super::graph_path;

/// Information about a dead agent
#[derive(Debug, Clone)]
pub struct DeadAgentInfo {
    pub agent_id: String,
    pub task_id: String,
    pub pid: u32,
    pub last_heartbeat: String,
    pub seconds_since_heartbeat: i64,
}

/// Result of dead agent detection
#[derive(Debug)]
pub struct DetectionResult {
    pub dead_agents: Vec<DeadAgentInfo>,
    pub tasks_unclaimed: Vec<String>,
    pub errors: Vec<String>,
}

/// Check for dead agents without modifying anything
pub fn run_check(dir: &Path, threshold_minutes: Option<u64>, json: bool) -> Result<()> {
    let config = Config::load(dir).unwrap_or_default();
    let threshold_mins = threshold_minutes.unwrap_or(config.agent.heartbeat_timeout);
    let threshold_secs = (threshold_mins * 60) as i64;

    let registry = AgentRegistry::load(dir)?;
    let dead_agents = registry.find_dead_agents(threshold_secs);

    if json {
        let output = serde_json::json!({
            "threshold_minutes": threshold_mins,
            "dead_count": dead_agents.len(),
            "dead_agents": dead_agents.iter().map(|a| {
                serde_json::json!({
                    "id": a.id,
                    "task_id": a.task_id,
                    "pid": a.pid,
                    "last_heartbeat": a.last_heartbeat,
                    "seconds_since_heartbeat": a.seconds_since_heartbeat(),
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "Dead agent check (threshold: {} minutes):\n",
            threshold_mins
        );

        if dead_agents.is_empty() {
            println!("No dead agents detected.");
        } else {
            println!("Found {} dead agent(s):", dead_agents.len());
            for agent in &dead_agents {
                let mins = agent.seconds_since_heartbeat().unwrap_or(0) / 60;
                println!(
                    "  {} - task '{}' (PID {}, last heartbeat {} min ago)",
                    agent.id, agent.task_id, agent.pid, mins
                );
            }
            println!();
            println!("Run 'wg dead-agents --cleanup' to mark as dead and unclaim tasks.");
        }
    }

    Ok(())
}

/// Detect dead agents, mark them as dead, and unclaim their tasks
pub fn run_cleanup(dir: &Path, threshold_minutes: Option<u64>, json: bool) -> Result<DetectionResult> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let config = Config::load(dir).unwrap_or_default();
    let threshold_mins = threshold_minutes.unwrap_or(config.agent.heartbeat_timeout);
    let threshold_secs = (threshold_mins * 60) as i64;

    // Load registry with lock
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    // Collect dead agent info before marking
    let dead_info: Vec<DeadAgentInfo> = locked_registry
        .find_dead_agents(threshold_secs)
        .iter()
        .filter_map(|a| {
            Some(DeadAgentInfo {
                agent_id: a.id.clone(),
                task_id: a.task_id.clone(),
                pid: a.pid,
                last_heartbeat: a.last_heartbeat.clone(),
                seconds_since_heartbeat: a.seconds_since_heartbeat()?,
            })
        })
        .collect();

    // Mark agents as dead
    let _dead_ids = locked_registry.mark_dead_agents(threshold_secs);

    // Save registry
    locked_registry.save_ref()?;

    // Now unclaim tasks from dead agents
    let mut graph = load_graph(&path).context("Failed to load graph")?;
    let mut tasks_unclaimed = Vec::new();
    let mut errors = Vec::new();

    for dead_agent in &dead_info {
        if let Some(task) = graph.get_task_mut(&dead_agent.task_id) {
            // Only unclaim if task is still in progress
            if task.status == Status::InProgress {
                task.status = Status::Open;
                task.assigned = None;
                // Don't clear started_at - keep the history

                // Add log entry
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: None,
                    message: format!(
                        "Task unclaimed: agent '{}' (PID {}) detected as dead (no heartbeat for {} seconds)",
                        dead_agent.agent_id,
                        dead_agent.pid,
                        dead_agent.seconds_since_heartbeat
                    ),
                });

                tasks_unclaimed.push(dead_agent.task_id.clone());
            }
        } else {
            errors.push(format!(
                "Task '{}' not found for dead agent '{}'",
                dead_agent.task_id, dead_agent.agent_id
            ));
        }
    }

    // Save graph
    if !tasks_unclaimed.is_empty() {
        save_graph(&graph, &path).context("Failed to save graph")?;
    }

    let result = DetectionResult {
        dead_agents: dead_info,
        tasks_unclaimed,
        errors,
    };

    // Output results
    if json {
        let output = serde_json::json!({
            "threshold_minutes": threshold_mins,
            "dead_agents_marked": result.dead_agents.len(),
            "tasks_unclaimed": result.tasks_unclaimed,
            "dead_agents": result.dead_agents.iter().map(|a| {
                serde_json::json!({
                    "id": a.agent_id,
                    "task_id": a.task_id,
                    "pid": a.pid,
                    "last_heartbeat": a.last_heartbeat,
                    "seconds_since_heartbeat": a.seconds_since_heartbeat,
                })
            }).collect::<Vec<_>>(),
            "errors": result.errors,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!(
            "Dead agent cleanup (threshold: {} minutes):\n",
            threshold_mins
        );

        if result.dead_agents.is_empty() {
            println!("No dead agents detected.");
        } else {
            println!(
                "Marked {} agent(s) as dead:",
                result.dead_agents.len()
            );
            for agent in &result.dead_agents {
                println!("  {} (PID {})", agent.agent_id, agent.pid);
            }

            if !result.tasks_unclaimed.is_empty() {
                println!();
                println!(
                    "Unclaimed {} task(s):",
                    result.tasks_unclaimed.len()
                );
                for task_id in &result.tasks_unclaimed {
                    println!("  {}", task_id);
                }
            }

            if !result.errors.is_empty() {
                println!();
                println!("Errors:");
                for err in &result.errors {
                    eprintln!("  {}", err);
                }
            }
        }
    }

    Ok(result)
}

/// Remove dead agents from the registry
pub fn run_remove_dead(dir: &Path, json: bool) -> Result<Vec<String>> {
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    // Find all dead agents
    let dead_ids: Vec<String> = locked_registry
        .list_agents()
        .iter()
        .filter(|a| a.status == AgentStatus::Dead)
        .map(|a| a.id.clone())
        .collect();

    // Remove them
    for id in &dead_ids {
        locked_registry.unregister_agent(id);
    }

    locked_registry.save()?;

    if json {
        let output = serde_json::json!({
            "removed": dead_ids,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if dead_ids.is_empty() {
            println!("No dead agents to remove.");
        } else {
            println!("Removed {} dead agent(s) from registry:", dead_ids.len());
            for id in &dead_ids {
                println!("  {}", id);
            }
        }
    }

    Ok(dead_ids)
}

/// Check if a process is still running
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    // Use kill with signal 0 to check if process exists
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> bool {
    // On non-Unix, assume process is alive
    true
}

/// Check for agents whose process has actually died
pub fn run_check_processes(dir: &Path, json: bool) -> Result<()> {
    let registry = AgentRegistry::load(dir)?;

    let mut dead_process_agents = Vec::new();
    let mut alive_agents = Vec::new();

    for agent in registry.list_alive_agents() {
        if is_process_alive(agent.pid) {
            alive_agents.push((agent.id.clone(), agent.task_id.clone(), agent.pid));
        } else {
            dead_process_agents.push((agent.id.clone(), agent.task_id.clone(), agent.pid));
        }
    }

    if json {
        let output = serde_json::json!({
            "alive": alive_agents.iter().map(|(id, task, pid)| {
                serde_json::json!({
                    "id": id,
                    "task_id": task,
                    "pid": pid,
                })
            }).collect::<Vec<_>>(),
            "dead_process": dead_process_agents.iter().map(|(id, task, pid)| {
                serde_json::json!({
                    "id": id,
                    "task_id": task,
                    "pid": pid,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Process check for registered agents:\n");

        if !alive_agents.is_empty() {
            println!("Alive ({}):", alive_agents.len());
            for (id, task, pid) in &alive_agents {
                println!("  {} on '{}' (PID {} running)", id, task, pid);
            }
        }

        if !dead_process_agents.is_empty() {
            println!();
            println!("Dead processes ({}):", dead_process_agents.len());
            for (id, task, pid) in &dead_process_agents {
                println!("  {} on '{}' (PID {} not running)", id, task, pid);
            }
            println!();
            println!("Run 'wg dead-agents --cleanup' to clean up these agents.");
        }

        if alive_agents.is_empty() && dead_process_agents.is_empty() {
            println!("No agents registered.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status,
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

    fn setup_with_agent_and_task() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        // Create graph with a task assigned to the agent
        let mut graph = WorkGraph::new();
        let mut task = make_task("task-1", "Test Task", Status::InProgress);
        task.assigned = Some("test-agent".to_string());
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        // Register an agent with an old heartbeat
        let mut registry = AgentRegistry::new();
        let agent_id = registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");
        // Set old heartbeat to simulate dead agent
        if let Some(agent) = registry.get_agent_mut(&agent_id) {
            agent.last_heartbeat = "2020-01-01T00:00:00Z".to_string();
        }
        registry.save(temp_dir.path()).unwrap();

        temp_dir
    }

    #[test]
    fn test_check_finds_dead_agents() {
        let temp_dir = setup_with_agent_and_task();

        // Should find the dead agent
        let result = run_check(temp_dir.path(), Some(1), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_cleanup_marks_dead_and_unclaims() {
        let temp_dir = setup_with_agent_and_task();

        // Run cleanup
        let result = run_cleanup(temp_dir.path(), Some(1), false);
        assert!(result.is_ok());

        let detection = result.unwrap();
        assert_eq!(detection.dead_agents.len(), 1);
        assert_eq!(detection.tasks_unclaimed.len(), 1);
        assert_eq!(detection.tasks_unclaimed[0], "task-1");

        // Verify agent is marked as dead
        let registry = AgentRegistry::load(temp_dir.path()).unwrap();
        let agent = registry.get_agent("agent-1").unwrap();
        assert_eq!(agent.status, AgentStatus::Dead);

        // Verify task is unclaimed
        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("task-1").unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
    }

    #[test]
    fn test_remove_dead() {
        let temp_dir = setup_with_agent_and_task();

        // First mark as dead
        run_cleanup(temp_dir.path(), Some(1), false).unwrap();

        // Now remove
        let result = run_remove_dead(temp_dir.path(), false);
        assert!(result.is_ok());

        let removed = result.unwrap();
        assert_eq!(removed.len(), 1);

        // Verify agent is gone
        let registry = AgentRegistry::load(temp_dir.path()).unwrap();
        assert!(registry.get_agent("agent-1").is_none());
    }

    #[test]
    fn test_no_dead_agents() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // Register an agent with fresh heartbeat
        let mut registry = AgentRegistry::new();
        registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");
        registry.save(temp_dir.path()).unwrap();

        // Check should find no dead agents
        let result = run_check(temp_dir.path(), Some(60), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_cleanup_no_dead_agents() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // Register an agent with fresh heartbeat
        let mut registry = AgentRegistry::new();
        registry.register_agent(12345, "task-1", "claude", "/tmp/output.log");
        registry.save(temp_dir.path()).unwrap();

        // Cleanup should do nothing
        let result = run_cleanup(temp_dir.path(), Some(60), false);
        assert!(result.is_ok());

        let detection = result.unwrap();
        assert!(detection.dead_agents.is_empty());
        assert!(detection.tasks_unclaimed.is_empty());
    }

    #[test]
    fn test_cleanup_json_output() {
        let temp_dir = setup_with_agent_and_task();

        // Should output valid JSON
        let result = run_cleanup(temp_dir.path(), Some(1), true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_processes() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // Register an agent with PID 1 (should exist on Unix)
        let mut registry = AgentRegistry::new();
        registry.register_agent(1, "task-1", "claude", "/tmp/output.log");
        registry.save(temp_dir.path()).unwrap();

        let result = run_check_processes(temp_dir.path(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_task_log_entry_on_unclaim() {
        let temp_dir = setup_with_agent_and_task();

        run_cleanup(temp_dir.path(), Some(1), false).unwrap();

        // Verify log entry was added
        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("task-1").unwrap();
        assert!(!task.log.is_empty());
        let log = task.log.last().unwrap();
        assert!(log.message.contains("dead"));
        assert!(log.message.contains("agent-1"));
    }
}
