use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::save_graph;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(dir: &Path, id: &str) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    let task = graph.get_task_mut_or_err(id)?;

    if task.status != Status::Failed {
        anyhow::bail!(
            "Task '{}' is not failed (status: {:?}). Only failed tasks can be retried.",
            id,
            task.status
        );
    }

    // Check if max retries exceeded
    if let Some(max) = task.max_retries
        && task.retry_count >= max
    {
        anyhow::bail!(
            "Task '{}' has reached max retries ({}/{}). Consider abandoning or increasing max_retries.",
            id,
            task.retry_count,
            max
        );
    }

    let prev_failure_reason = task.failure_reason.clone();
    let attempt = task.retry_count + 1;

    task.status = Status::Open;
    // Keep retry_count for history - don't reset it
    // Clear failure_reason since we're retrying
    task.failure_reason = None;
    // Clear assigned so the coordinator can re-spawn an agent
    task.assigned = None;

    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: None,
        message: format!("Task reset for retry (attempt #{})", task.retry_count + 1),
    });

    // Extract values we need for printing before saving
    let retry_count = task.retry_count;
    let max_retries = task.max_retries;

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "retry",
        Some(id),
        None,
        serde_json::json!({ "attempt": attempt, "prev_failure_reason": prev_failure_reason }),
        config.log.rotation_threshold,
    );

    println!(
        "Reset '{}' to open for retry (attempt #{})",
        id,
        retry_count + 1
    );

    if let Some(max) = max_retries {
        println!("  Retries remaining after this: {}", max - retry_count);
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
    fn test_retry_failed_task_transitions_to_open() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.failure_reason = Some("timeout".to_string());
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
    }

    #[test]
    fn test_retry_non_failed_task_errors_open() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not failed"),
            "Expected 'not failed' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_retry_non_failed_task_errors_in_progress() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::InProgress)],
        );

        let result = run(dir_path, "t1");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not failed"));
    }

    #[test]
    fn test_retry_non_failed_task_errors_done() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Done)]);

        let result = run(dir_path, "t1");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not failed"));
    }

    #[test]
    fn test_retry_preserves_retry_count() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 3;
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1").unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.retry_count, 3,
            "retry_count should be preserved, not reset"
        );
    }

    #[test]
    fn test_retry_clears_failure_reason() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.failure_reason = Some("compilation error".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1").unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.failure_reason, None);
    }

    #[test]
    fn test_retry_clears_assigned() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1").unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.assigned, None);
    }

    #[test]
    fn test_retry_max_retries_exceeded() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 3;
        task.max_retries = Some(3);
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("max retries"),
            "Expected 'max retries' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_retry_within_max_retries_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.max_retries = Some(3);
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
    }

    #[test]
    fn test_retry_adds_log_entry() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 2;
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1").unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(!task.log.is_empty());
        let last_log = task.log.last().unwrap();
        assert!(
            last_log.message.contains("retry"),
            "Log message should mention retry, got: {}",
            last_log.message
        );
        assert!(
            last_log.message.contains("3"),
            "Log message should contain attempt number 3, got: {}",
            last_log.message
        );
    }

    #[test]
    fn test_retry_task_not_found() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Failed)]);

        let result = run(dir_path, "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}
