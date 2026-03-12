//! Sweep: detect and recover orphaned in-progress tasks.
//!
//! An "orphaned" task is one that is InProgress but whose assigned agent is
//! dead (process exited, marked Dead in registry, or missing entirely).
//! This can happen due to the split-save race condition in cleanup_dead_agents()
//! where the registry save succeeds but the graph save is overwritten by a
//! concurrent writer.
//!
//! `wg sweep` is a user-friendly, idempotent command that:
//! - Detects all orphaned in-progress tasks
//! - Unclaims them (resets to Open) so the service re-dispatches
//! - Reports what it found and fixed
//!
//! Usage:
//!   wg sweep              # Detect and fix orphaned tasks
//!   wg sweep --dry-run    # Just report, don't modify

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;

use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};
use workgraph::service::registry::{AgentRegistry, AgentStatus};

use super::{graph_path, is_process_alive};

/// Information about an orphaned task found by sweep
#[derive(Debug, Clone)]
pub struct OrphanedTask {
    pub task_id: String,
    pub task_title: String,
    pub assigned_agent: String,
    pub reason: String,
}

/// Result of a sweep operation
#[derive(Debug)]
pub struct SweepResult {
    pub orphaned: Vec<OrphanedTask>,
    pub fixed: Vec<String>,
}

/// Detect orphaned in-progress tasks whose agents are dead or missing.
///
/// This is the reconciliation safety net that catches tasks missed by the
/// split-save race in cleanup_dead_agents().
pub fn find_orphaned_tasks(dir: &Path) -> Result<Vec<OrphanedTask>> {
    let gpath = graph_path(dir);
    let graph = load_graph(&gpath).context("Failed to load graph")?;
    let registry = AgentRegistry::load(dir).unwrap_or_else(|_| AgentRegistry::new());

    let mut orphaned = Vec::new();

    for task in graph.tasks() {
        if task.status != Status::InProgress {
            continue;
        }

        let agent_id = match &task.assigned {
            Some(id) => id,
            None => {
                // InProgress but no agent assigned — orphaned
                orphaned.push(OrphanedTask {
                    task_id: task.id.clone(),
                    task_title: task.title.clone(),
                    assigned_agent: "(none)".to_string(),
                    reason: "InProgress with no assigned agent".to_string(),
                });
                continue;
            }
        };

        match registry.get_agent(agent_id) {
            Some(agent) => {
                if agent.status == AgentStatus::Dead {
                    orphaned.push(OrphanedTask {
                        task_id: task.id.clone(),
                        task_title: task.title.clone(),
                        assigned_agent: agent_id.clone(),
                        reason: format!("Agent '{}' is marked Dead in registry", agent_id),
                    });
                } else if agent.is_alive() && !is_process_alive(agent.pid) {
                    orphaned.push(OrphanedTask {
                        task_id: task.id.clone(),
                        task_title: task.title.clone(),
                        assigned_agent: agent_id.clone(),
                        reason: format!(
                            "Agent '{}' (PID {}) process is not running",
                            agent_id, agent.pid
                        ),
                    });
                }
            }
            None => {
                // Agent not in registry at all — orphaned
                orphaned.push(OrphanedTask {
                    task_id: task.id.clone(),
                    task_title: task.title.clone(),
                    assigned_agent: agent_id.clone(),
                    reason: format!("Agent '{}' not found in registry", agent_id),
                });
            }
        }
    }

    Ok(orphaned)
}

/// Run sweep: detect and fix orphaned tasks.
/// If `dry_run` is true, only reports without modifying.
pub fn run(dir: &Path, dry_run: bool, json: bool) -> Result<SweepResult> {
    let orphaned = find_orphaned_tasks(dir)?;

    if orphaned.is_empty() {
        if json {
            let output = serde_json::json!({
                "orphaned_count": 0,
                "fixed": [],
                "dry_run": dry_run,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("No orphaned tasks found. Everything looks clean.");
        }
        return Ok(SweepResult {
            orphaned: vec![],
            fixed: vec![],
        });
    }

    if dry_run {
        if json {
            let output = serde_json::json!({
                "dry_run": true,
                "orphaned_count": orphaned.len(),
                "orphaned": orphaned.iter().map(|o| serde_json::json!({
                    "task_id": o.task_id,
                    "title": o.task_title,
                    "assigned_agent": o.assigned_agent,
                    "reason": o.reason,
                })).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!(
                "Found {} orphaned task(s) (dry run, no changes made):\n",
                orphaned.len()
            );
            for o in &orphaned {
                println!("  {} — \"{}\"", o.task_id, o.task_title);
                println!("    Reason: {}", o.reason);
            }
            println!();
            println!("Run 'wg sweep' (without --dry-run) to fix these tasks.");
        }
        return Ok(SweepResult {
            orphaned,
            fixed: vec![],
        });
    }

    // Fix orphaned tasks: unclaim them
    let gpath = graph_path(dir);
    let mut graph = load_graph(&gpath).context("Failed to load graph")?;
    let mut fixed = Vec::new();

    for o in &orphaned {
        if let Some(task) = graph.get_task_mut(&o.task_id) {
            if task.status == Status::InProgress {
                task.status = Status::Open;
                task.assigned = None;
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("sweep".to_string()),
                    message: format!("Sweep: task unclaimed — {}", o.reason),
                });
                fixed.push(o.task_id.clone());
            }
        }
    }

    if !fixed.is_empty() {
        save_graph(&graph, &gpath).context("Failed to save graph")?;
        super::notify_graph_changed(dir);
    }

    if json {
        let output = serde_json::json!({
            "dry_run": false,
            "orphaned_count": orphaned.len(),
            "fixed": fixed,
            "orphaned": orphaned.iter().map(|o| serde_json::json!({
                "task_id": o.task_id,
                "title": o.task_title,
                "assigned_agent": o.assigned_agent,
                "reason": o.reason,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Sweep complete:\n");
        println!("Found {} orphaned task(s):", orphaned.len());
        for o in &orphaned {
            println!("  {} — \"{}\"", o.task_id, o.task_title);
            println!("    Reason: {}", o.reason);
        }
        if !fixed.is_empty() {
            println!();
            println!("Fixed {} task(s) (reset to Open):", fixed.len());
            for id in &fixed {
                println!("  {}", id);
            }
        }
    }

    Ok(SweepResult { orphaned, fixed })
}

/// Reconciliation function for use inside the coordinator tick.
/// Scans for InProgress tasks whose assigned agent is Dead in the registry
/// and resets them to Open. Returns the number of tasks recovered.
///
/// This is the safety net for Bug 1 (split-save ordering) described in the
/// root cause analysis.
pub fn reconcile_orphaned_tasks(dir: &Path, graph_path: &Path) -> Result<usize> {
    let registry = AgentRegistry::load(dir).unwrap_or_else(|_| AgentRegistry::new());
    let mut graph = load_graph(graph_path).context("Failed to load graph")?;

    // First pass: collect IDs of orphaned tasks (can't mutate while iterating)
    let orphaned_ids: Vec<(String, String)> = graph
        .tasks()
        .filter(|task| task.status == Status::InProgress)
        .filter_map(|task| {
            let dominated = match &task.assigned {
                Some(agent_id) => match registry.get_agent(agent_id) {
                    Some(agent) => {
                        agent.status == AgentStatus::Dead
                            || (agent.is_alive() && !is_process_alive(agent.pid))
                    }
                    None => {
                        // Agent not in registry — could be purged. Only treat as
                        // orphaned if the task has been InProgress for a while
                        // (to avoid racing with a freshly-spawned agent that
                        // hasn't registered yet).
                        if let Some(ref started) = task.started_at {
                            if let Ok(started_dt) = started.parse::<chrono::DateTime<chrono::Utc>>()
                            {
                                (Utc::now() - started_dt).num_minutes() > 5
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    }
                },
                None => {
                    // System coordinator/compact tasks are managed by the daemon
                    // directly and never have an assigned agent — skip them.
                    !task
                        .tags
                        .iter()
                        .any(|t| t == "coordinator-loop" || t == "compact-loop")
                }
            };

            if dominated {
                let agent_desc = task.assigned.as_deref().unwrap_or("(none)").to_string();
                Some((task.id.clone(), agent_desc))
            } else {
                None
            }
        })
        .collect();

    // Second pass: mutate the orphaned tasks
    let count = orphaned_ids.len();
    for (task_id, agent_desc) in &orphaned_ids {
        if let Some(task) = graph.get_task_mut(task_id) {
            task.status = Status::Open;
            task.assigned = None;
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("reconcile".to_string()),
                message: format!(
                    "Reconciliation: task recovered from orphaned state (agent: {})",
                    agent_desc
                ),
            });
        }
    }

    if count > 0 {
        save_graph(&graph, graph_path).context("Failed to save graph after reconciliation")?;
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::save_graph;
    use workgraph::service::registry::AgentRegistry;

    fn make_task(id: &str, title: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..Task::default()
        }
    }

    fn setup_graph_and_registry(
        dir: &Path,
        tasks: Vec<Task>,
        agents: Vec<(&str, &str, u32, AgentStatus)>,
    ) {
        let gpath = dir.join("graph.jsonl");
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &gpath).unwrap();

        let mut registry = AgentRegistry::new();
        for (agent_id_suffix, task_id, pid, status) in agents {
            let id = registry.register_agent(pid, task_id, "claude", "/tmp/output.log");
            // The registry auto-generates IDs, so we need to set the status
            if let Some(agent) = registry.get_agent_mut(&id) {
                match status {
                    AgentStatus::Dead => {
                        agent.status = AgentStatus::Dead;
                        agent.completed_at = Some(Utc::now().to_rfc3339());
                    }
                    other => agent.status = other,
                }
            }
            // We can't control the generated ID easily, so we'll use
            // a different approach for tests
            let _ = agent_id_suffix; // suppress unused warning
        }
        registry.save(dir).unwrap();
    }

    /// Helper that creates a graph with a specific agent ID assignment
    fn setup_with_dead_agent(dir: &Path) {
        let gpath = dir.join("graph.jsonl");
        let mut graph = WorkGraph::new();

        let mut task = make_task("stuck-task", "Stuck Task", Status::InProgress);
        task.assigned = Some("dead-agent-1".to_string());
        graph.add_node(Node::Task(task));

        let mut task2 = make_task("healthy-task", "Healthy Task", Status::InProgress);
        task2.assigned = Some("alive-agent-1".to_string());
        graph.add_node(Node::Task(task2));

        let mut task3 = make_task("done-task", "Done Task", Status::Done);
        task3.assigned = Some("dead-agent-2".to_string());
        graph.add_node(Node::Task(task3));

        save_graph(&graph, &gpath).unwrap();

        // Create registry with one dead agent and one that's missing
        let mut registry = AgentRegistry::new();

        // Manually insert a dead agent entry
        use workgraph::service::registry::AgentEntry;
        registry.agents.insert(
            "dead-agent-1".to_string(),
            AgentEntry {
                id: "dead-agent-1".to_string(),
                pid: 99999,
                task_id: "stuck-task".to_string(),
                executor: "claude".to_string(),
                status: AgentStatus::Dead,
                started_at: Utc::now().to_rfc3339(),
                last_heartbeat: "2020-01-01T00:00:00Z".to_string(),
                completed_at: Some(Utc::now().to_rfc3339()),
                output_file: "/tmp/output.log".to_string(),
                model: None,
            },
        );

        // alive-agent-1 is NOT in registry (simulates purged agent)
        // but we won't catch it unless it's been >5 min

        registry.save(dir).unwrap();
    }

    #[test]
    fn test_sweep_detects_dead_agents() {
        let temp_dir = TempDir::new().unwrap();
        setup_with_dead_agent(temp_dir.path());

        let orphaned = find_orphaned_tasks(temp_dir.path()).unwrap();

        // Should find stuck-task (dead agent in registry)
        let stuck = orphaned.iter().find(|o| o.task_id == "stuck-task");
        assert!(stuck.is_some(), "Should detect task with dead agent");
        assert!(
            stuck.unwrap().reason.contains("Dead"),
            "Reason should mention Dead status"
        );

        // Should NOT find done-task (it's Done, not InProgress)
        assert!(
            orphaned.iter().all(|o| o.task_id != "done-task"),
            "Should not flag Done tasks"
        );
    }

    #[test]
    fn test_sweep_unclaims_stuck() {
        let temp_dir = TempDir::new().unwrap();
        setup_with_dead_agent(temp_dir.path());

        let result = run(temp_dir.path(), false, false).unwrap();

        assert!(!result.fixed.is_empty(), "Should fix at least one task");
        assert!(
            result.fixed.contains(&"stuck-task".to_string()),
            "Should fix stuck-task"
        );

        // Verify the task is now Open
        let gpath = graph_path(temp_dir.path());
        let graph = load_graph(&gpath).unwrap();
        let task = graph.get_task("stuck-task").unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
        assert!(task.log.last().unwrap().message.contains("Sweep"));
    }

    #[test]
    fn test_sweep_dry_run_does_not_modify() {
        let temp_dir = TempDir::new().unwrap();
        setup_with_dead_agent(temp_dir.path());

        let result = run(temp_dir.path(), true, false).unwrap();

        assert!(!result.orphaned.is_empty(), "Should detect orphans");
        assert!(result.fixed.is_empty(), "Dry run should not fix anything");

        // Verify task is still InProgress
        let gpath = graph_path(temp_dir.path());
        let graph = load_graph(&gpath).unwrap();
        let task = graph.get_task("stuck-task").unwrap();
        assert_eq!(task.status, Status::InProgress);
    }

    #[test]
    fn test_sweep_no_orphans() {
        let temp_dir = TempDir::new().unwrap();
        let gpath = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(
            "open-task",
            "Open Task",
            Status::Open,
        )));
        graph.add_node(Node::Task(make_task(
            "done-task",
            "Done Task",
            Status::Done,
        )));
        save_graph(&graph, &gpath).unwrap();

        let result = run(temp_dir.path(), false, false).unwrap();
        assert!(result.orphaned.is_empty());
        assert!(result.fixed.is_empty());
    }

    #[test]
    fn test_sweep_inprogress_no_agent() {
        let temp_dir = TempDir::new().unwrap();
        let gpath = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        // InProgress but no assigned agent
        graph.add_node(Node::Task(make_task(
            "no-agent",
            "No Agent Task",
            Status::InProgress,
        )));
        save_graph(&graph, &gpath).unwrap();

        let orphaned = find_orphaned_tasks(temp_dir.path()).unwrap();
        assert_eq!(orphaned.len(), 1);
        assert_eq!(orphaned[0].task_id, "no-agent");
        assert!(orphaned[0].reason.contains("no assigned agent"));
    }

    #[test]
    fn test_reconcile_orphaned_tasks() {
        let temp_dir = TempDir::new().unwrap();
        setup_with_dead_agent(temp_dir.path());

        let gpath = temp_dir.path().join("graph.jsonl");
        let count = reconcile_orphaned_tasks(temp_dir.path(), &gpath).unwrap();

        assert!(count >= 1, "Should reconcile at least one task");

        // Verify task is now Open
        let graph = load_graph(&gpath).unwrap();
        let task = graph.get_task("stuck-task").unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
        assert!(task.log.last().unwrap().message.contains("Reconciliation"));
    }

    #[test]
    fn test_sweep_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        setup_with_dead_agent(temp_dir.path());

        // First sweep
        let result1 = run(temp_dir.path(), false, false).unwrap();
        assert!(!result1.fixed.is_empty());

        // Second sweep — should find nothing
        let result2 = run(temp_dir.path(), false, false).unwrap();
        assert!(result2.orphaned.is_empty());
        assert!(result2.fixed.is_empty());
    }

    #[test]
    fn test_sweep_json_output() {
        let temp_dir = TempDir::new().unwrap();
        setup_with_dead_agent(temp_dir.path());

        // Should not panic with json output
        let result = run(temp_dir.path(), false, true).unwrap();
        assert!(!result.fixed.is_empty());
    }
}
