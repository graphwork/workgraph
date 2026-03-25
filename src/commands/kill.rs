//! Kill running agents
//!
//! Terminates agent processes and cleans up their registry entries.
//!
//! Usage:
//!   wg kill agent-1           # Graceful kill (SIGTERM, wait, SIGKILL)
//!   wg kill agent-1 --force   # Force kill (SIGKILL immediately)
//!   wg kill --all             # Kill all running agents

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::modify_graph;
use workgraph::service::{AgentRegistry, AgentStatus};

use super::{graph_path, kill_process_force, kill_process_graceful};

/// Default wait time between SIGTERM and SIGKILL
const DEFAULT_WAIT_SECS: u64 = 5;

/// Kill a single agent
pub fn run(dir: &Path, agent_id: &str, force: bool, json: bool) -> Result<()> {
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    let agent = locked_registry
        .get_agent(agent_id)
        .ok_or_else(|| anyhow::anyhow!("Agent '{}' not found", agent_id))?;

    let pid = agent.pid;
    let task_id = agent.task_id.clone();

    // Kill the process
    if force {
        kill_process_force(pid)?;
    } else {
        kill_process_graceful(pid, DEFAULT_WAIT_SECS)?;
    }

    // Update registry
    locked_registry.update_status(agent_id, AgentStatus::Stopping)?;
    locked_registry.save_ref()?;

    // Unclaim the task
    unclaim_task(dir, &task_id, agent_id)?;

    // Remove agent from registry
    locked_registry.unregister_agent(agent_id);
    locked_registry.save()?;

    if json {
        let output = serde_json::json!({
            "killed": agent_id,
            "pid": pid,
            "task_id": task_id,
            "force": force,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if force {
            println!("Force killed {} (PID {})", agent_id, pid);
        } else {
            println!("Killed {} (PID {})", agent_id, pid);
        }
        println!("Task '{}' unclaimed", task_id);
    }

    Ok(())
}

/// Kill all running agents
pub fn run_all(dir: &Path, force: bool, json: bool) -> Result<()> {
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    // Get all alive agents
    let alive_agents: Vec<_> = locked_registry
        .list_alive_agents()
        .iter()
        .map(|a| (a.id.clone(), a.pid, a.task_id.clone()))
        .collect();

    if alive_agents.is_empty() {
        if json {
            let output = serde_json::json!({
                "killed": [],
                "count": 0,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("No running agents to kill.");
        }
        return Ok(());
    }

    let mut killed = Vec::new();
    let mut errors = Vec::new();

    for (agent_id, pid, task_id) in &alive_agents {
        // Kill the process
        let kill_result = if force {
            kill_process_force(*pid)
        } else {
            kill_process_graceful(*pid, DEFAULT_WAIT_SECS)
        };

        if let Err(e) = kill_result {
            errors.push(format!("{}: {}", agent_id, e));
            continue;
        }

        // Update status
        if let Err(e) = locked_registry.update_status(agent_id, AgentStatus::Stopping) {
            eprintln!(
                "Warning: failed to update status for agent {}: {}",
                agent_id, e
            );
        }

        // Unclaim task
        if let Err(e) = unclaim_task(dir, task_id, agent_id) {
            errors.push(format!("Failed to unclaim task '{}': {}", task_id, e));
            // Don't unregister: agent entry needed so the task can be linked back
            continue;
        }

        // Remove from registry only after successful unclaim
        locked_registry.unregister_agent(agent_id);

        killed.push((agent_id.clone(), *pid, task_id.clone()));
    }

    locked_registry.save()?;

    if json {
        let output = serde_json::json!({
            "killed": killed.iter().map(|(id, pid, task)| {
                serde_json::json!({
                    "id": id,
                    "pid": pid,
                    "task_id": task,
                })
            }).collect::<Vec<_>>(),
            "count": killed.len(),
            "errors": errors,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if killed.is_empty() {
            println!("No agents were killed.");
        } else {
            println!("Killed {} agent(s):", killed.len());
            for (id, pid, task) in &killed {
                println!("  {} (PID {}) - task '{}'", id, pid, task);
            }
        }

        if !errors.is_empty() {
            eprintln!();
            eprintln!("Errors:");
            for err in &errors {
                eprintln!("  {}", err);
            }
        }
    }

    Ok(())
}

/// Unclaim the task that was being worked on by the killed agent
fn unclaim_task(dir: &Path, task_id: &str, agent_id: &str) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        return Ok(()); // No graph, nothing to unclaim
    }

    modify_graph(&path, |graph| {
        if let Some(task) = graph.get_task_mut(task_id) {
            if task.status == Status::InProgress {
                task.status = Status::Open;
                task.assigned = None;

                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: None,
                    user: Some(workgraph::current_user()),
                    message: format!("Task unclaimed: agent '{}' was killed", agent_id),
                });

                return true;
            }
        }
        false
    })
    .context("Failed to modify graph")?;

    super::notify_graph_changed(dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::save_graph;
    use workgraph::service::is_process_alive;

    fn make_task(id: &str, title: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..Task::default()
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

        // Register an agent with a fake PID (use PID 1 which should always exist on Unix)
        let mut registry = AgentRegistry::new();
        registry.register_agent(1, "task-1", "claude", "/tmp/output.log");
        registry.save(temp_dir.path()).unwrap();

        temp_dir
    }

    #[test]
    fn test_is_process_alive() {
        // Current process should always be running
        #[cfg(unix)]
        {
            let pid = std::process::id();
            assert!(is_process_alive(pid));
        }

        // Random high PID likely doesn't exist
        #[cfg(unix)]
        assert!(!is_process_alive(999999999));
    }

    #[test]
    fn test_unclaim_task() {
        let temp_dir = setup_with_agent_and_task();

        // Unclaim the task
        let result = unclaim_task(temp_dir.path(), "task-1", "agent-1");
        assert!(result.is_ok());

        // Verify task is unclaimed
        let graph = load_graph(graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("task-1").unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
        assert!(!task.log.is_empty());
    }

    #[test]
    fn test_unclaim_task_not_in_progress() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        // Create graph with a done task
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("task-1", "Done Task", Status::Done)));
        save_graph(&graph, &path).unwrap();

        // Unclaim should succeed but not change anything
        let result = unclaim_task(temp_dir.path(), "task-1", "agent-1");
        assert!(result.is_ok());

        // Verify task is still done
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("task-1").unwrap();
        assert_eq!(task.status, Status::Done);
    }

    #[test]
    fn test_run_all_empty() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // No agents registered
        let result = run_all(temp_dir.path(), false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_kill_agent_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "agent-999", false, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    // Note: Can't easily test actual process killing in unit tests
    // as it would require spawning real processes. The kill functions
    // are tested manually or in integration tests.
}
