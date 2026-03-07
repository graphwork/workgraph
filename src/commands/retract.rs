//! retract command — provenance-based undo of a task's side effects.
//!
//! Finds all tasks created by agents that worked on the target task (transitively),
//! kills their agents, abandons them leaf-first, and resets the original task to open.

use anyhow::Result;
use chrono::Utc;
use std::collections::HashSet;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::service::AgentRegistry;

/// Run retract: undo side effects of a task by tracing provenance lineage.
pub fn run(dir: &Path, id: &str, abandon: bool, dry_run: bool, no_kill: bool) -> Result<()> {
    // Verify the target task exists
    {
        let (graph, _) = super::load_workgraph(dir)?;
        graph.get_task_or_err(id)?;
    }

    // Transitively find all tasks created by agents working on this task
    let retract_targets = find_retract_targets(dir, id)?;

    if dry_run {
        println!("Dry run — retraction plan for '{}':", id);
        if retract_targets.is_empty() {
            println!("  (no provenance-traced created tasks found)");
        } else {
            for tid in &retract_targets {
                println!("  abandon: {}", tid);
            }
        }
        if abandon {
            println!("  abandon (original): {}", id);
        } else {
            println!("  reset to open: {}", id);
        }
        return Ok(());
    }

    // Kill agents on all affected tasks (unless --no-kill)
    if !no_kill {
        // Kill agents on retract targets
        for tid in &retract_targets {
            kill_agent_for_task(dir, tid);
        }
        // Kill agent on the original task too
        kill_agent_for_task(dir, id);
    }

    // Abandon retract targets in leaf-first order
    // Sort in reverse so deeper tasks (added later, typically leaves) come first
    let mut ordered = retract_targets.clone();
    order_leaf_first(dir, &mut ordered);

    let mut abandoned_count = 0;
    for tid in &ordered {
        // Use mutate_workgraph for each to handle already-abandoned gracefully
        let result = super::mutate_workgraph(dir, |graph| {
            let task = graph.get_task_mut_or_err(tid)?;
            if task.status == Status::Done || task.status == Status::Abandoned {
                return Ok(false);
            }
            task.status = Status::Abandoned;
            task.failure_reason = Some(format!("retracted via `wg retract {}`", id));
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: None,
                message: format!("Task abandoned via `wg retract {}`", id),
                ..Default::default()
            });
            Ok(true)
        });
        match result {
            Ok(true) => abandoned_count += 1,
            Ok(false) => {} // already terminal
            Err(_) => {}    // task might have been removed
        }
    }

    // Reset or abandon the original task
    let original_action = if abandon {
        super::mutate_workgraph(dir, |graph| {
            let task = graph.get_task_mut_or_err(id)?;
            if task.status != Status::Abandoned {
                task.status = Status::Abandoned;
                task.failure_reason = Some("retracted via `wg retract --abandon`".to_string());
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: None,
                    message: "Task abandoned via `wg retract --abandon`".to_string(),
                    ..Default::default()
                });
            }
            Ok(())
        })?;
        "abandoned"
    } else {
        super::mutate_workgraph(dir, |graph| {
            let task = graph.get_task_mut_or_err(id)?;
            task.status = Status::Open;
            task.assigned = None;
            task.started_at = None;
            task.completed_at = None;
            task.failure_reason = None;
            task.session_id = None;
            task.checkpoint = None;
            task.wait_condition = None;
            task.paused = false;
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: None,
                message: "Task reset to open via `wg retract`".to_string(),
                ..Default::default()
            });
            Ok(())
        })?;
        "reset to open"
    };

    super::notify_graph_changed(dir);

    // Record provenance
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "retract",
        Some(id),
        None,
        serde_json::json!({
            "abandon": abandon,
            "no_kill": no_kill,
            "retracted_tasks": ordered,
            "abandoned_count": abandoned_count,
            "original_action": original_action,
        }),
        config.log.rotation_threshold,
    );

    // Print results
    for tid in &ordered {
        println!("  retracted: {}", tid);
    }
    println!(
        "Retracted '{}': {} task(s) abandoned, original {}",
        id, abandoned_count, original_action
    );

    Ok(())
}

/// Transitively find all tasks created by agents that worked on the target task.
///
/// Algorithm:
/// 1. Find agents that claimed/worked on `task_id`
/// 2. Find tasks created by those agents (via provenance `add_task` entries)
/// 3. Recurse: for each created task, find its agents and their created tasks
/// 4. Return the full transitive set (excluding the original task)
fn find_retract_targets(dir: &Path, task_id: &str) -> Result<Vec<String>> {
    let mut all_created = HashSet::new();
    let mut frontier: HashSet<String> = HashSet::new();
    frontier.insert(task_id.to_string());

    loop {
        // Find agents for the current frontier of tasks
        let agents = workgraph::provenance::find_agents_for_tasks(dir, &frontier)?;
        if agents.is_empty() {
            break;
        }

        // Find tasks created by those agents
        let created = workgraph::provenance::find_tasks_created_by_agents(dir, &agents)?;

        // Filter to only new tasks we haven't seen
        let mut new_frontier = HashSet::new();
        for tid in created {
            if tid != task_id && all_created.insert(tid.clone()) {
                new_frontier.insert(tid);
            }
        }

        if new_frontier.is_empty() {
            break;
        }

        frontier = new_frontier;
    }

    let mut result: Vec<String> = all_created.into_iter().collect();
    result.sort();
    Ok(result)
}

/// Order tasks leaf-first by building a dependency graph and sorting.
/// Tasks with no dependents in the set come first.
fn order_leaf_first(dir: &Path, tasks: &mut Vec<String>) {
    let Ok((graph, _)) = super::load_workgraph(dir) else {
        return; // fallback: keep current order
    };

    let task_set: HashSet<String> = tasks.iter().cloned().collect();

    // Build in-set dependency count: how many tasks in our set depend on each task
    let mut dep_count: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for tid in &task_set {
        dep_count.insert(tid.clone(), 0);
    }

    for tid in &task_set {
        if let Some(task) = graph.get_task(tid) {
            for dep in &task.after {
                if task_set.contains(dep) {
                    *dep_count.entry(dep.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    // Sort: tasks with fewer in-set dependents first (leaves first)
    tasks.sort_by(|a, b| {
        let ca = dep_count.get(a).copied().unwrap_or(0);
        let cb = dep_count.get(b).copied().unwrap_or(0);
        ca.cmp(&cb).then(a.cmp(b))
    });
}

/// Best-effort kill of an agent working on a given task.
fn kill_agent_for_task(dir: &Path, task_id: &str) {
    let Ok(registry) = AgentRegistry::load(dir) else {
        return;
    };
    let Some(agent) = registry.get_agent_by_task(task_id) else {
        return;
    };
    if !agent.is_alive() {
        return;
    }
    let pid = agent.pid;
    let _ = super::kill_process_graceful(pid, 5);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::{load_graph, save_graph};

    fn graph_path(dir: &Path) -> std::path::PathBuf {
        dir.join("graph.jsonl")
    }

    fn make_task(id: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            status,
            ..Task::default()
        }
    }

    fn setup(dir: &Path, tasks: Vec<Task>) {
        fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
    }

    /// Write provenance entries to simulate agent activity.
    fn write_provenance(dir: &Path, entries: Vec<workgraph::provenance::OperationEntry>) {
        for entry in entries {
            workgraph::provenance::append_operation(
                dir,
                &entry,
                workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
            )
            .unwrap();
        }
    }

    fn make_claim_entry(task_id: &str, agent_id: &str) -> workgraph::provenance::OperationEntry {
        workgraph::provenance::OperationEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            op: "claim".to_string(),
            task_id: Some(task_id.to_string()),
            actor: Some(agent_id.to_string()),
            detail: serde_json::json!({"prev_status": "Open"}),
        }
    }

    fn make_add_task_entry(
        task_id: &str,
        agent_id: &str,
    ) -> workgraph::provenance::OperationEntry {
        workgraph::provenance::OperationEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            op: "add_task".to_string(),
            task_id: Some(task_id.to_string()),
            actor: None,
            detail: serde_json::json!({"title": task_id, "agent_id": agent_id}),
        }
    }

    #[test]
    fn test_retract_finds_provenance_traced_tasks() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Setup: original task + 2 tasks created by agent-1
        let original = make_task("original", Status::InProgress);
        let child1 = make_task("child-1", Status::Open);
        let child2 = make_task("child-2", Status::Open);
        setup(dir, vec![original, child1, child2]);

        // Provenance: agent-1 claimed original, then created child-1 and child-2
        write_provenance(
            dir,
            vec![
                make_claim_entry("original", "agent-1"),
                make_add_task_entry("child-1", "agent-1"),
                make_add_task_entry("child-2", "agent-1"),
            ],
        );

        let targets = find_retract_targets(dir, "original").unwrap();
        assert!(targets.contains(&"child-1".to_string()));
        assert!(targets.contains(&"child-2".to_string()));
        assert!(!targets.contains(&"original".to_string()));
    }

    #[test]
    fn test_retract_transitive() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // original -> (agent-1 creates child-1) -> (agent-2 claims child-1, creates grandchild)
        let original = make_task("original", Status::InProgress);
        let child = make_task("child-1", Status::InProgress);
        let grandchild = make_task("grandchild", Status::Open);
        setup(dir, vec![original, child, grandchild]);

        write_provenance(
            dir,
            vec![
                make_claim_entry("original", "agent-1"),
                make_add_task_entry("child-1", "agent-1"),
                make_claim_entry("child-1", "agent-2"),
                make_add_task_entry("grandchild", "agent-2"),
            ],
        );

        let targets = find_retract_targets(dir, "original").unwrap();
        assert!(targets.contains(&"child-1".to_string()));
        assert!(targets.contains(&"grandchild".to_string()));
    }

    #[test]
    fn test_retract_abandons_leaf_first() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let original = make_task("original", Status::InProgress);
        let mut child = make_task("child", Status::Open);
        child.after = vec!["original".to_string()];
        let mut grandchild = make_task("grandchild", Status::Open);
        grandchild.after = vec!["child".to_string()];
        setup(dir, vec![original, child, grandchild]);

        write_provenance(
            dir,
            vec![
                make_claim_entry("original", "agent-1"),
                make_add_task_entry("child", "agent-1"),
                make_add_task_entry("grandchild", "agent-1"),
            ],
        );

        run(dir, "original", false, false, true).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("grandchild").unwrap().status, Status::Abandoned);
        assert_eq!(graph.get_task("child").unwrap().status, Status::Abandoned);
        assert_eq!(graph.get_task("original").unwrap().status, Status::Open);
    }

    #[test]
    fn test_retract_resets_original_to_open() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mut original = make_task("original", Status::InProgress);
        original.assigned = Some("agent-1".to_string());
        setup(dir, vec![original]);

        write_provenance(dir, vec![make_claim_entry("original", "agent-1")]);

        run(dir, "original", false, false, true).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        let task = graph.get_task("original").unwrap();
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.assigned, None);
    }

    #[test]
    fn test_retract_abandon_flag() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let original = make_task("original", Status::InProgress);
        setup(dir, vec![original]);

        write_provenance(dir, vec![make_claim_entry("original", "agent-1")]);

        run(dir, "original", true, false, true).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("original").unwrap().status, Status::Abandoned);
    }

    #[test]
    fn test_retract_dry_run() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let original = make_task("original", Status::InProgress);
        let child = make_task("child", Status::Open);
        setup(dir, vec![original, child]);

        write_provenance(
            dir,
            vec![
                make_claim_entry("original", "agent-1"),
                make_add_task_entry("child", "agent-1"),
            ],
        );

        run(dir, "original", false, true, true).unwrap();

        // Nothing should have changed
        let graph = load_graph(&graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("original").unwrap().status, Status::InProgress);
        assert_eq!(graph.get_task("child").unwrap().status, Status::Open);
    }

    #[test]
    fn test_retract_records_provenance() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let original = make_task("original", Status::InProgress);
        let child = make_task("child", Status::Open);
        setup(dir, vec![original, child]);

        write_provenance(
            dir,
            vec![
                make_claim_entry("original", "agent-1"),
                make_add_task_entry("child", "agent-1"),
            ],
        );

        run(dir, "original", false, false, true).unwrap();

        let entries = workgraph::provenance::read_all_operations(dir).unwrap();
        let retract_ops: Vec<_> = entries.iter().filter(|e| e.op == "retract").collect();
        assert_eq!(retract_ops.len(), 1);
        assert_eq!(retract_ops[0].task_id.as_deref(), Some("original"));
        let retracted = retract_ops[0].detail["retracted_tasks"].as_array().unwrap();
        assert!(retracted.iter().any(|v| v.as_str() == Some("child")));
    }

    #[test]
    fn test_retract_no_provenance_targets() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        setup(dir, vec![make_task("lonely", Status::Open)]);

        run(dir, "lonely", false, false, true).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("lonely").unwrap().status, Status::Open);
    }

    #[test]
    fn test_retract_nonexistent_task_errors() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Open)]);

        let err = run(dir, "nonexistent", false, false, true).unwrap_err();
        assert!(format!("{:#}", err).contains("not found"));
    }
}
