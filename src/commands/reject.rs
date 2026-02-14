//! Reject command - send a pending-review task back for rework
//!
//! Used by reviewers to reject work submitted by agents.
//! Sets the task back to Open so it can be claimed and reworked.

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

pub fn run(dir: &Path, task_id: &str, reason: Option<&str>, actor: Option<&str>) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Only allow reject from PendingReview
    if task.status != Status::PendingReview {
        anyhow::bail!(
            "Cannot reject task '{}': status is {:?}, expected PendingReview",
            task_id,
            task.status
        );
    }

    // Set status back to Open for rework
    task.status = Status::Open;
    task.assigned = None;
    task.retry_count += 1;

    // Add log entry
    let message = match reason {
        Some(r) => format!("Work rejected: {}", r),
        None => "Work rejected (no reason given)".to_string(),
    };
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: actor.map(String::from),
        message,
    });

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    println!("Rejected task '{}' - returned to open for rework", task_id);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
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
    fn test_reject_pending_review_to_open() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        let mut task = make_task("t1", "Test task", Status::PendingReview);
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(&dir, vec![task]);

        run(&dir, "t1", Some("needs more work"), Some("reviewer")).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let t = graph.get_task("t1").unwrap();
        assert_eq!(t.status, Status::Open);
    }

    #[test]
    fn test_reject_clears_assigned() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        let mut task = make_task("t1", "Test task", Status::PendingReview);
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(&dir, vec![task]);

        run(&dir, "t1", None, None).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let t = graph.get_task("t1").unwrap();
        assert_eq!(t.assigned, None);
    }

    #[test]
    fn test_reject_increments_retry_count() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        let mut task = make_task("t1", "Test task", Status::PendingReview);
        task.retry_count = 2;
        setup_workgraph(&dir, vec![task]);

        run(&dir, "t1", None, None).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let t = graph.get_task("t1").unwrap();
        assert_eq!(t.retry_count, 3);
    }

    #[test]
    fn test_reject_stores_reason_in_log() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        let task = make_task("t1", "Test task", Status::PendingReview);
        setup_workgraph(&dir, vec![task]);

        run(&dir, "t1", Some("Tests are failing"), Some("reviewer")).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let t = graph.get_task("t1").unwrap();
        assert_eq!(t.log.len(), 1);
        assert!(t.log[0].message.contains("Tests are failing"));
        assert_eq!(t.log[0].actor, Some("reviewer".to_string()));
    }

    #[test]
    fn test_reject_without_reason_logs_default() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        let task = make_task("t1", "Test task", Status::PendingReview);
        setup_workgraph(&dir, vec![task]);

        run(&dir, "t1", None, None).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let t = graph.get_task("t1").unwrap();
        assert!(t.log[0].message.contains("no reason given"));
    }

    #[test]
    fn test_reject_error_on_non_pending_review() {
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

            let result = run(&dir, "t1", None, None);
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
    fn test_reject_error_on_nonexistent_task() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        setup_workgraph(&dir, vec![]);

        let result = run(&dir, "nonexistent", None, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}
