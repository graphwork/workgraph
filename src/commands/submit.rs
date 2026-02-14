//! Submit command - mark work complete, awaiting review
//!
//! For tasks with --verify set, agents must use submit instead of done.
//! This sets the task to PendingReview status.

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::agency::capture_task_output;
use workgraph::graph::{LogEntry, Status};
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

    // Only allow submit from InProgress
    if task.status != Status::InProgress {
        anyhow::bail!(
            "Cannot submit task '{}': status is {:?}, expected InProgress",
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
            "Cannot submit task '{}': blocked by {} unresolved task(s):\n{}",
            task_id,
            blockers.len(),
            blocker_list.join("\n")
        );
    }

    // Re-acquire mutable reference after immutable borrow
    let task = graph.get_task_mut(task_id).unwrap();

    // Set status to PendingReview
    task.status = Status::PendingReview;

    // Add log entry
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: actor.map(String::from),
        message: "Work submitted for review".to_string(),
    });

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    println!("Submitted task '{}' for review", task_id);
    if let Some(ref verify) = graph.get_task(task_id).and_then(|t| t.verify.clone()) {
        println!("Verification criteria: {}", verify);
    }

    // Capture task output (git diff, artifacts, log) for evaluation.
    // When auto_evaluate is enabled, the coordinator creates an evaluation task
    // in the graph that becomes ready once this task completes; the captured
    // output feeds that evaluator.
    if let Some(task) = graph.get_task(task_id) {
        match capture_task_output(dir, task) {
            Ok(output_dir) => {
                eprintln!("Output captured to {}", output_dir.display());
            }
            Err(e) => {
                eprintln!("Warning: output capture failed: {}", e);
            }
        }
    }

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
    fn test_submit_sets_pending_review() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.verify = Some("Check output".to_string());
        setup_workgraph(&dir, vec![task]);

        run(&dir, "t1", Some("agent-1")).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let t = graph.get_task("t1").unwrap();
        assert_eq!(t.status, Status::PendingReview);
    }

    #[test]
    fn test_submit_creates_log_entry() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        let task = make_task("t1", "Test task", Status::InProgress);
        setup_workgraph(&dir, vec![task]);

        run(&dir, "t1", Some("agent-1")).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let t = graph.get_task("t1").unwrap();
        assert_eq!(t.log.len(), 1);
        assert_eq!(t.log[0].message, "Work submitted for review");
        assert_eq!(t.log[0].actor, Some("agent-1".to_string()));
    }

    #[test]
    fn test_submit_error_on_non_in_progress() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");

        for status in [
            Status::Open,
            Status::PendingReview,
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
                err.contains("expected InProgress"),
                "Error for {:?} should mention InProgress: {}",
                status,
                err
            );
        }
    }

    #[test]
    fn test_submit_error_on_nonexistent_task() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        setup_workgraph(&dir, vec![]);

        let result = run(&dir, "nonexistent", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_submit_error_when_blocked() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");

        let blocker = make_task("blocker", "Blocker", Status::Open);
        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.blocked_by = vec!["blocker".to_string()];

        setup_workgraph(&dir, vec![blocker, task]);

        let result = run(&dir, "t1", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("blocked by"));
    }

    #[test]
    fn test_submit_without_actor() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".workgraph");
        let task = make_task("t1", "Test task", Status::InProgress);
        setup_workgraph(&dir, vec![task]);

        run(&dir, "t1", None).unwrap();

        let graph = load_graph(graph_path(&dir)).unwrap();
        let t = graph.get_task("t1").unwrap();
        assert_eq!(t.status, Status::PendingReview);
        assert_eq!(t.log[0].actor, None);
    }
}
