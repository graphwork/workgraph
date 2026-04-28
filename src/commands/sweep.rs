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
use workgraph::parser::{load_graph, modify_graph};
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
#[allow(dead_code)]
pub struct SweepResult {
    pub orphaned: Vec<OrphanedTask>,
    pub fixed: Vec<String>,
    pub targets_reaped: usize,
    pub bytes_freed: u64,
}

/// Detect orphaned tasks whose agents are dead, missing, or unreachable.
///
/// Covers two shapes of orphaning:
/// - `Status::InProgress` with a dead/missing agent (the original
///   split-save race in `cleanup_dead_agents()`).
/// - `Status::Open` with a dead/missing-agent claim — see
///   `bug-retry-doesnt-clear-stale-downstream-claims`. These tasks are
///   never going to be picked up by the dispatcher because its "ready"
///   check excludes assigned tasks. Surfacing them in `wg sweep` lets
///   users diagnose why the dispatcher silently stalled on a fan-out.
pub fn find_orphaned_tasks(dir: &Path) -> Result<Vec<OrphanedTask>> {
    let gpath = graph_path(dir);
    let graph = load_graph(&gpath).context("Failed to load graph")?;
    let registry = AgentRegistry::load(dir).unwrap_or_else(|_| AgentRegistry::new());

    let mut orphaned = Vec::new();

    for task in graph.tasks() {
        let is_inprogress = task.status == Status::InProgress;
        let is_open_with_claim = task.status == Status::Open && task.assigned.is_some();
        if !is_inprogress && !is_open_with_claim {
            continue;
        }

        let agent_id = match &task.assigned {
            Some(id) => id,
            None => {
                // InProgress but no agent assigned — orphaned (split-save).
                orphaned.push(OrphanedTask {
                    task_id: task.id.clone(),
                    task_title: task.title.clone(),
                    assigned_agent: "(none)".to_string(),
                    reason: "InProgress with no assigned agent".to_string(),
                });
                continue;
            }
        };

        let status_label = if is_inprogress { "InProgress" } else { "Open" };
        match registry.get_agent(agent_id) {
            Some(agent) => {
                if agent.status == AgentStatus::Dead {
                    orphaned.push(OrphanedTask {
                        task_id: task.id.clone(),
                        task_title: task.title.clone(),
                        assigned_agent: agent_id.clone(),
                        reason: format!(
                            "{} task; agent '{}' is marked Dead in registry",
                            status_label, agent_id
                        ),
                    });
                } else if agent.is_alive() && !is_process_alive(agent.pid) {
                    orphaned.push(OrphanedTask {
                        task_id: task.id.clone(),
                        task_title: task.title.clone(),
                        assigned_agent: agent_id.clone(),
                        reason: format!(
                            "{} task; agent '{}' (PID {}) process is not running",
                            status_label, agent_id, agent.pid
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
                    reason: format!(
                        "{} task; agent '{}' not found in registry",
                        status_label, agent_id
                    ),
                });
            }
        }
    }

    Ok(orphaned)
}

/// Run sweep: detect and fix orphaned tasks.
/// If `dry_run` is true, only reports without modifying.
/// If `reap_targets` is true, also removes `target/` build artifacts
/// from worktrees of agents that are no longer live (skipped under
/// `dry_run`).
pub fn run(dir: &Path, dry_run: bool, reap_targets: bool, json: bool) -> Result<SweepResult> {
    let orphaned = find_orphaned_tasks(dir)?;

    let (targets_reaped, bytes_freed) = if reap_targets && !dry_run {
        match crate::commands::service::worktree::reap_dead_target_dirs(dir) {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("Warning: target-dir reap failed: {}", e);
                (0, 0)
            }
        }
    } else {
        (0, 0)
    };

    if orphaned.is_empty() {
        if json {
            let output = serde_json::json!({
                "orphaned_count": 0,
                "fixed": [],
                "dry_run": dry_run,
                "targets_reaped": targets_reaped,
                "bytes_freed": bytes_freed,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else if reap_targets {
            if dry_run {
                println!(
                    "No orphaned tasks. Dry run: target reap skipped — re-run without --dry-run to reap."
                );
            } else if targets_reaped == 0 {
                println!("No orphaned tasks found. No target/ dirs reaped.");
            } else {
                println!(
                    "No orphaned tasks. Reaped target/ in {} dead worktree(s), freed {} bytes.",
                    targets_reaped, bytes_freed
                );
            }
        } else {
            println!("No orphaned tasks found. Everything looks clean.");
        }
        return Ok(SweepResult {
            orphaned: vec![],
            fixed: vec![],
            targets_reaped,
            bytes_freed,
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
            targets_reaped,
            bytes_freed,
        });
    }

    // Archive each orphaned agent's output BEFORE unclaiming the task, so
    // the attempt is preserved in `.workgraph/log/agents/<task-id>/<timestamp>/`
    // and visible in the TUI iteration switcher. Without this, an in-progress
    // task that gets respawned via sweep loses prior attempts from the
    // iteration history. Best-effort — failures are non-fatal. Skip the
    // sentinel "(none)" placeholder used when assigned is None.
    for o in &orphaned {
        if o.assigned_agent == "(none)" {
            continue;
        }
        if let Err(e) = super::log::archive_agent(dir, &o.task_id, &o.assigned_agent) {
            eprintln!(
                "Warning: failed to archive orphaned agent '{}' for task '{}': {}",
                o.assigned_agent, o.task_id, e
            );
        }
    }

    // Fix orphaned tasks: unclaim them
    let gpath = graph_path(dir);
    let mut fixed = Vec::new();

    let orphaned_clone = orphaned.clone();
    modify_graph(&gpath, |graph| {
        let mut modified = false;
        for o in &orphaned_clone {
            if let Some(task) = graph.get_task_mut(&o.task_id)
                && matches!(task.status, Status::InProgress | Status::Open)
                && task.assigned.is_some()
            {
                task.status = Status::Open;
                task.assigned = None;
                task.started_at = None;
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("sweep".to_string()),
                    user: Some(workgraph::current_user()),
                    message: format!("Sweep: task unclaimed — {}", o.reason),
                });
                fixed.push(o.task_id.clone());
                modified = true;
            } else if let Some(task) = graph.get_task_mut(&o.task_id)
                && task.status == Status::InProgress
                && task.assigned.is_none()
            {
                // The "(none)" sentinel branch: InProgress with no assigned.
                // Just transition to Open; nothing to clear.
                task.status = Status::Open;
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("sweep".to_string()),
                    user: Some(workgraph::current_user()),
                    message: format!("Sweep: task unclaimed — {}", o.reason),
                });
                fixed.push(o.task_id.clone());
                modified = true;
            }
        }
        modified
    })
    .context("Failed to modify graph")?;
    if !fixed.is_empty() {
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
            "targets_reaped": targets_reaped,
            "bytes_freed": bytes_freed,
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
        if reap_targets {
            println!();
            if targets_reaped == 0 {
                println!("Target reap: no dead-agent target/ dirs found.");
            } else {
                println!(
                    "Target reap: cleared {} target/ dir(s), freed {} bytes.",
                    targets_reaped, bytes_freed
                );
            }
        }
    }

    Ok(SweepResult {
        orphaned,
        fixed,
        targets_reaped,
        bytes_freed,
    })
}

/// Reconciliation function for use inside the coordinator tick.
/// Scans for tasks in `InProgress` OR `Open` whose assigned agent is Dead
/// (or unreachable, or absent from the registry) and clears the stale
/// claim. `InProgress` tasks transition to Open; `Open` tasks just have
/// their claim wiped — both end up dispatchable on the next tick.
/// Returns the number of tasks recovered.
///
/// This is the safety net the eager paths (`wg reset`, `wg retry`) cannot
/// cover: kill -9, OOM, panic-before-cleanup, host reboot. The eager
/// paths handle user-initiated transitions with low latency; this lazy
/// path catches everything else once per dispatcher tick (poll_interval).
///
/// Originally only handled `Status::InProgress`. The `Open + assigned-but-
/// dead` predicate was added to fix `bug-retry-doesnt-clear-stale-
/// downstream-claims`: synthesis tasks that were never claimed by a live
/// agent (the agency assigner stamped an `assigned` value, then the
/// upstream agent died before reaching them) used to sit there forever
/// because the previous filter `status == InProgress` skipped them.
pub fn reconcile_orphaned_tasks(dir: &Path, graph_path: &Path) -> Result<usize> {
    let registry = AgentRegistry::load(dir).unwrap_or_else(|_| AgentRegistry::new());

    let mut count = 0usize;
    modify_graph(graph_path, |graph| {
        // First pass: collect IDs of orphaned tasks (status + reason).
        let orphaned_ids: Vec<(String, Status, String)> = graph
            .tasks()
            .filter(|task| matches!(task.status, Status::InProgress | Status::Open))
            .filter_map(|task| {
                let dominated = match &task.assigned {
                    Some(agent_id) => match registry.get_agent(agent_id) {
                        Some(agent) => {
                            agent.status == AgentStatus::Dead
                                || (agent.is_alive() && !is_process_alive(agent.pid))
                        }
                        None => {
                            // Agent absent from registry. For InProgress we
                            // require >5min since started_at to avoid races
                            // with a freshly-spawned agent that hasn't
                            // written its registry entry yet. For Open we
                            // can act immediately — `started_at` is None
                            // for an Open task that was never picked up,
                            // and a missing-agent claim means whatever
                            // process owned it is unrecoverable.
                            if task.status == Status::Open {
                                true
                            } else if let Some(ref started) = task.started_at {
                                if let Ok(started_dt) =
                                    started.parse::<chrono::DateTime<chrono::Utc>>()
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
                        // Status=Open with no assigned is normal — skip.
                        // Status=InProgress with no assigned IS orphaned
                        // (split-save race), unless this is a long-lived
                        // loop task without an agent (coordinator/compact).
                        task.status == Status::InProgress
                            && !task.tags.iter().any(|t| {
                                t == "coordinator-loop" || t == "compact-loop"
                            })
                    }
                };

                if dominated {
                    let agent_desc = task.assigned.as_deref().unwrap_or("(none)").to_string();
                    Some((task.id.clone(), task.status, agent_desc))
                } else {
                    None
                }
            })
            .collect();

        // Second pass: mutate the orphaned tasks.
        count = orphaned_ids.len();
        for (task_id, prev_status, agent_desc) in &orphaned_ids {
            if let Some(task) = graph.get_task_mut(task_id) {
                let was_open = *prev_status == Status::Open;
                task.status = Status::Open;
                task.assigned = None;
                // started_at only matters for InProgress; clearing it on
                // an Open task is a no-op, but explicit is fine.
                task.started_at = None;
                let kind = if was_open {
                    "stale-claim cleared"
                } else {
                    "task recovered from orphaned state"
                };
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: Some("reconcile".to_string()),
                    user: Some(workgraph::current_user()),
                    message: format!(
                        "Reconciliation: {} (was {:?}, agent: {})",
                        kind, prev_status, agent_desc
                    ),
                });
            }
        }
        count > 0
    })
    .context("Failed to modify graph after reconciliation")?;

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

    #[allow(dead_code)]
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
            worktree_path: None,
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

        let result = run(temp_dir.path(), false, false, false).unwrap();

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

        let result = run(temp_dir.path(), true, false, false).unwrap();

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

        let result = run(temp_dir.path(), false, false, false).unwrap();
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
        let result1 = run(temp_dir.path(), false, false, false).unwrap();
        assert!(!result1.fixed.is_empty());

        // Second sweep — should find nothing
        let result2 = run(temp_dir.path(), false, false, false).unwrap();
        assert!(result2.orphaned.is_empty());
        assert!(result2.fixed.is_empty());
    }

    #[test]
    fn test_sweep_json_output() {
        let temp_dir = TempDir::new().unwrap();
        setup_with_dead_agent(temp_dir.path());

        // Should not panic with json output
        let result = run(temp_dir.path(), false, false, true).unwrap();
        assert!(!result.fixed.is_empty());
    }

    #[test]
    fn test_sweep_reap_targets_flag_exposes_count() {
        // worktree-target-dirs: --reap-targets surfaces dead-agent target/
        // dirs and removes them via the wg sweep CLI. We don't try to
        // construct a real worktree here (covered in worktree.rs unit
        // tests); we just verify the plumbing returns a SweepResult with
        // populated target_reaped/bytes_freed fields when invoked, and
        // returns zero when there is nothing to reap.
        let temp_dir = TempDir::new().unwrap();
        save_graph(&WorkGraph::new(), &temp_dir.path().join("graph.jsonl")).unwrap();

        // No worktrees, no orphaned tasks → should return zero counts.
        let result = run(temp_dir.path(), false, true, false).unwrap();
        assert_eq!(result.targets_reaped, 0);
        assert_eq!(result.bytes_freed, 0);
    }

    #[test]
    fn test_sweep_dry_run_skips_target_reap() {
        // worktree-target-dirs: --dry-run + --reap-targets must NOT
        // mutate the filesystem, even if real target/ dirs were present.
        let temp_dir = TempDir::new().unwrap();
        save_graph(&WorkGraph::new(), &temp_dir.path().join("graph.jsonl")).unwrap();
        let result = run(temp_dir.path(), true, true, false).unwrap();
        assert_eq!(
            result.targets_reaped, 0,
            "dry-run must not reap any target/ dirs"
        );
    }

    /// Regression test for tui-cannot-view: when sweep unclaims an orphaned
    /// task, the now-dead agent's output.log must be archived to
    /// `.workgraph/log/agents/<task-id>/<timestamp>/` so the TUI iteration
    /// switcher can show that attempt.
    #[test]
    fn test_sweep_archives_orphaned_agent_for_iteration_history() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        setup_with_dead_agent(dir);

        // Simulate the killed agent's working dir, populated with the
        // partial output that was written before the stream hung.
        let agent_dir = dir.join("agents").join("dead-agent-1");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("output.log"),
            "stream hung — sweep unclaimed",
        )
        .unwrap();
        std::fs::write(agent_dir.join("prompt.txt"), "task prompt").unwrap();

        let result = run(dir, false, false, false).unwrap();
        assert!(result.fixed.contains(&"stuck-task".to_string()));

        // Verify the archive landed where TUI's find_all_archives() looks.
        let archive_base = dir.join("log").join("agents").join("stuck-task");
        assert!(
            archive_base.exists(),
            "Per-task archive dir must be created when sweep unclaims agent"
        );
        let archives: Vec<_> = std::fs::read_dir(&archive_base)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_ok_and(|ft| ft.is_dir()))
            .collect();
        assert_eq!(archives.len(), 1);
        let archived = std::fs::read_to_string(archives[0].path().join("output.txt")).unwrap();
        assert!(archived.contains("stream hung"));
    }

    /// TDD for the lazy reconciler path described in design-claim-lifecycle:
    /// a task in `Status::Open` (not InProgress!) whose `assigned` references
    /// a Dead agent must be unclaimed by the next dispatcher tick. This is
    /// the safety net that catches what `wg reset`/`wg retry` eager paths
    /// miss (kill -9, panic, host reboot).
    #[test]
    fn test_dispatcher_heartbeat_unclaims_dead_agents() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let gpath = dir.join("graph.jsonl");

        // Open task assigned to a dead agent — would have been silently
        // skipped by the dispatcher pre-fix (status==InProgress filter).
        let mut t = make_task("ready-task", "Ready Task", Status::Open);
        t.assigned = Some("agent-zombie-1".to_string());
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(t));
        save_graph(&graph, &gpath).unwrap();

        // Registry: dead agent.
        use workgraph::service::registry::AgentEntry;
        let mut reg = AgentRegistry::new();
        reg.agents.insert(
            "agent-zombie-1".to_string(),
            AgentEntry {
                id: "agent-zombie-1".to_string(),
                pid: 99999,
                task_id: "ready-task".to_string(),
                executor: "claude".to_string(),
                status: AgentStatus::Dead,
                started_at: Utc::now().to_rfc3339(),
                last_heartbeat: "2020-01-01T00:00:00Z".to_string(),
                completed_at: Some(Utc::now().to_rfc3339()),
                output_file: "/tmp/output.log".to_string(),
                model: None,
                worktree_path: None,
            },
        );
        reg.save(dir).unwrap();

        // Simulate one dispatcher tick.
        let count = reconcile_orphaned_tasks(dir, &gpath).unwrap();
        assert!(count >= 1, "reconciler must cover Open + dead-agent claims");

        let graph = load_graph(&gpath).unwrap();
        let task = graph.get_task("ready-task").unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(
            task.assigned.is_none(),
            "lazy reconciler must wipe the stale claim so dispatcher can pick it up"
        );
        assert!(
            task.log.iter().any(|e| e.message.contains("Reconciliation")),
            "log entry should record reconciler action: {:?}",
            task.log.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }

    /// Reconciler must NOT touch an Open task with a still-alive agent
    /// claim. (Possible if assignment was made but the agent hasn't yet
    /// flipped the task to InProgress.)
    #[test]
    fn test_reconciler_preserves_open_tasks_with_live_claims() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let gpath = dir.join("graph.jsonl");

        let mut t = make_task("warming-up", "Warming up", Status::Open);
        t.assigned = Some("agent-alive-1".to_string());
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(t));
        save_graph(&graph, &gpath).unwrap();

        use workgraph::service::registry::AgentEntry;
        let mut reg = AgentRegistry::new();
        reg.agents.insert(
            "agent-alive-1".to_string(),
            AgentEntry {
                id: "agent-alive-1".to_string(),
                pid: std::process::id(),
                task_id: "warming-up".to_string(),
                executor: "claude".to_string(),
                status: AgentStatus::Working,
                started_at: Utc::now().to_rfc3339(),
                last_heartbeat: Utc::now().to_rfc3339(),
                completed_at: None,
                output_file: "/tmp/output.log".to_string(),
                model: None,
                worktree_path: None,
            },
        );
        reg.save(dir).unwrap();

        let _ = reconcile_orphaned_tasks(dir, &gpath).unwrap();

        let graph = load_graph(&gpath).unwrap();
        let task = graph.get_task("warming-up").unwrap();
        assert_eq!(task.assigned, Some("agent-alive-1".to_string()));
    }

    /// Regression test for tui-cannot-view: orphaned tasks where assigned was
    /// already None (sentinel "(none)") should not crash sweep when archiving
    /// — there is no agent dir to archive, so the call must be skipped.
    #[test]
    fn test_sweep_skips_archive_for_unassigned_orphan() {
        let temp_dir = TempDir::new().unwrap();
        let gpath = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task(
            "no-agent",
            "No Agent Task",
            Status::InProgress,
        )));
        save_graph(&graph, &gpath).unwrap();

        // Should not panic / error even though agent dir doesn't exist.
        let result = run(temp_dir.path(), false, false, false).unwrap();
        assert!(result.fixed.contains(&"no-agent".to_string()));

        // No archive should be created for the "(none)" sentinel.
        let archive_base = temp_dir.path().join("log").join("agents").join("no-agent");
        assert!(
            !archive_base.exists(),
            "No archive for sentinel-only orphan"
        );
    }
}
