use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::save_graph;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

/// Reject a task that is pending validation, reopening it with feedback.
/// If rejection_count >= max_rejections, the task is failed instead.
pub fn run(dir: &Path, id: &str, reason: &str) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    let task = graph.get_task_mut_or_err(id)?;

    if task.status != Status::PendingValidation {
        anyhow::bail!(
            "Task '{}' is not pending validation (status: {:?}). Only pending-validation tasks can be rejected.",
            id,
            task.status
        );
    }

    task.rejection_count += 1;
    let rejection_count = task.rejection_count;
    let max_rejections = task.max_rejections.unwrap_or(3);

    if rejection_count >= max_rejections {
        // Too many rejections — fail the task
        task.status = Status::Failed;
        task.failure_reason = Some(format!(
            "Exceeded max rejections ({}/{}): {}",
            rejection_count, max_rejections, reason
        ));
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: std::env::var("WG_AGENT_ID").ok(),
            message: format!(
                "Task rejected ({}/{}), failing: {}",
                rejection_count, max_rejections, reason
            ),
        });

        save_graph(&graph, &path).context("Failed to save graph")?;
        super::notify_graph_changed(dir);

        let config = workgraph::config::Config::load_or_default(dir);
        let _ = workgraph::provenance::record(
            dir,
            "reject",
            Some(id),
            std::env::var("WG_AGENT_ID").ok().as_deref(),
            serde_json::json!({
                "reason": reason,
                "rejection_count": rejection_count,
                "max_rejections": max_rejections,
                "outcome": "failed",
            }),
            config.log.rotation_threshold,
        );

        println!(
            "Rejected '{}' ({}/{} rejections) — task failed",
            id, rejection_count, max_rejections
        );
    } else {
        // Reopen for re-dispatch
        task.status = Status::Open;
        task.assigned = None;
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: std::env::var("WG_AGENT_ID").ok(),
            message: format!(
                "Task rejected ({}/{}): {}",
                rejection_count, max_rejections, reason
            ),
        });

        save_graph(&graph, &path).context("Failed to save graph")?;
        super::notify_graph_changed(dir);

        let config = workgraph::config::Config::load_or_default(dir);
        let _ = workgraph::provenance::record(
            dir,
            "reject",
            Some(id),
            std::env::var("WG_AGENT_ID").ok().as_deref(),
            serde_json::json!({
                "reason": reason,
                "rejection_count": rejection_count,
                "max_rejections": max_rejections,
                "outcome": "reopened",
            }),
            config.log.rotation_threshold,
        );

        println!(
            "Rejected '{}' ({}/{} rejections) — reopened for re-dispatch",
            id, rejection_count, max_rejections
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Task, WorkGraph};

    fn make_task(id: &str, title: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..Task::default()
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
    fn test_reject_reopens_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::PendingValidation)],
        );

        let result = run(dir_path, "t1", "Tests fail");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.rejection_count, 1);
        assert!(task.assigned.is_none());
    }

    #[test]
    fn test_reject_adds_feedback_to_log() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::PendingValidation)],
        );

        run(dir_path, "t1", "3 test failures in auth module").unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        let last_log = task.log.last().unwrap();
        assert!(last_log.message.contains("3 test failures in auth module"));
        assert!(last_log.message.contains("rejected"));
    }

    #[test]
    fn test_reject_max_rejections_fails_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::PendingValidation);
        task.rejection_count = 2;
        task.max_rejections = Some(3);
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", "Still broken");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
        assert!(
            task.failure_reason
                .as_ref()
                .unwrap()
                .contains("max rejections")
        );
    }

    #[test]
    fn test_reject_within_max_reopens() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::PendingValidation);
        task.rejection_count = 1;
        task.max_rejections = Some(3);
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", "Minor issue").unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.rejection_count, 2);
    }

    #[test]
    fn test_reject_non_pending_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", "reason");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not pending validation"));
    }

    #[test]
    fn test_reject_clears_assigned() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::PendingValidation);
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", "needs rework").unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.assigned.is_none());
    }

    #[test]
    fn test_reject_nonexistent_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![]);

        let result = run(dir_path, "nonexistent", "reason");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_reject_default_max_rejections_is_3() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::PendingValidation);
        // No max_rejections set — should default to 3
        task.rejection_count = 2;
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", "third strike").unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
    }
}
