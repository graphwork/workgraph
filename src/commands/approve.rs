//! Approve command - mark a pending-review task as done
//!
//! Used by reviewers to approve work submitted by agents.

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status, evaluate_loop_edges};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query;

use super::graph_path;

pub fn run(dir: &Path, task_id: &str, actor: Option<&str>) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Only allow approve from PendingReview
    if task.status != Status::PendingReview {
        anyhow::bail!(
            "Cannot approve task '{}': status is {:?}, expected PendingReview",
            task_id,
            task.status
        );
    }

    // Check for unresolved blockers
    let blockers = query::blocked_by(&graph, task_id);
    if !blockers.is_empty() {
        let blocker_list: Vec<String> = blockers
            .iter()
            .map(|t| format!("  - {} ({}): {:?}", t.id, t.title, t.status))
            .collect();
        anyhow::bail!(
            "Cannot approve task '{}': blocked by {} unresolved task(s):\n{}",
            task_id,
            blockers.len(),
            blocker_list.join("\n")
        );
    }

    // Re-acquire mutable reference after immutable borrow
    let task = graph.get_task_mut(task_id).unwrap();

    // Set status to Done
    task.status = Status::Done;
    task.completed_at = Some(Utc::now().to_rfc3339());

    // Add log entry
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: actor.map(String::from),
        message: "Work approved and marked done".to_string(),
    });

    // Evaluate loop edges: re-activate upstream tasks if conditions are met
    let id_owned = task_id.to_string();
    let reactivated = evaluate_loop_edges(&mut graph, &id_owned);

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    println!("Approved task '{}' - now done", task_id);

    for tid in &reactivated {
        println!("  Loop: re-activated '{}'", tid);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{LoopEdge, Node, Task, WorkGraph};
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
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
        }
    }

    fn setup_workgraph(dir: &Path, tasks: Vec<Task>) -> std::path::PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
        path
    }

    #[test]
    fn test_approve_pending_review_to_done() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        let task = make_task("t1", "Test task", Status::PendingReview);
        setup_workgraph(&dir, vec![task]);

        run(&dir, "t1", Some("reviewer")).unwrap();

        let graph = load_graph(&graph_path(&dir)).unwrap();
        let t = graph.get_task("t1").unwrap();
        assert_eq!(t.status, Status::Done);
        assert!(t.completed_at.is_some());
    }

    #[test]
    fn test_approve_creates_log_entry() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        let task = make_task("t1", "Test task", Status::PendingReview);
        setup_workgraph(&dir, vec![task]);

        run(&dir, "t1", Some("reviewer")).unwrap();

        let graph = load_graph(&graph_path(&dir)).unwrap();
        let t = graph.get_task("t1").unwrap();
        assert_eq!(t.log.len(), 1);
        assert_eq!(t.log[0].message, "Work approved and marked done");
        assert_eq!(t.log[0].actor, Some("reviewer".to_string()));
    }

    #[test]
    fn test_approve_error_on_non_pending_review() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");

        for status in [
            Status::Open,
            Status::InProgress,
            Status::Done,
            Status::Failed,
            Status::Blocked,
        ] {
            let task = make_task("t1", "Test task", status.clone());
            setup_workgraph(&dir, vec![task]);

            let result = run(&dir, "t1", None);
            assert!(result.is_err(), "Expected error for status {:?}", status);
            let err = result.unwrap_err().to_string();
            assert!(
                err.contains("expected PendingReview"),
                "Error for {:?} should mention PendingReview: {}",
                status,
                err
            );
        }
    }

    #[test]
    fn test_approve_error_on_nonexistent_task() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        setup_workgraph(&dir, vec![]);

        let result = run(&dir, "nonexistent", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_approve_error_when_blocked() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");

        let blocker = make_task("blocker", "Blocker", Status::Open);
        let mut task = make_task("t1", "Test task", Status::PendingReview);
        task.blocked_by = vec!["blocker".to_string()];

        setup_workgraph(&dir, vec![blocker, task]);

        let result = run(&dir, "t1", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("blocked by"));
    }

    #[test]
    fn test_approve_triggers_loop_edge_reactivation() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");

        // target_task is Done (will be re-activated by loop)
        let target = make_task("target", "Loop target", Status::Done);

        // source_task has a loop_edge pointing to target
        let mut source = make_task("source", "Loop source", Status::PendingReview);
        source.loops_to = vec![LoopEdge {
            target: "target".to_string(),
            guard: None,
            max_iterations: 3,
            delay: None,
        }];

        setup_workgraph(&dir, vec![target, source]);

        run(&dir, "source", None).unwrap();

        let graph = load_graph(&graph_path(&dir)).unwrap();
        let target = graph.get_task("target").unwrap();
        assert_eq!(
            target.status,
            Status::Open,
            "Target should be re-activated to Open"
        );
        assert_eq!(target.loop_iteration, 1);
    }

    #[test]
    fn test_approve_without_actor() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        let task = make_task("t1", "Test task", Status::PendingReview);
        setup_workgraph(&dir, vec![task]);

        run(&dir, "t1", None).unwrap();

        let graph = load_graph(&graph_path(&dir)).unwrap();
        let t = graph.get_task("t1").unwrap();
        assert_eq!(t.status, Status::Done);
        assert_eq!(t.log[0].actor, None);
    }
}
