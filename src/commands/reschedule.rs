use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use std::path::Path;
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

pub fn run(
    dir: &Path,
    id: &str,
    after_hours: Option<f64>,
    at_timestamp: Option<&str>,
) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;

    let new_timestamp = if let Some(hours) = after_hours {
        // Calculate timestamp as now + hours
        let duration = Duration::seconds((hours * 3600.0) as i64);
        let future_time = Utc::now() + duration;
        future_time.to_rfc3339()
    } else if let Some(timestamp) = at_timestamp {
        // Validate the timestamp
        timestamp
            .parse::<chrono::DateTime<Utc>>()
            .context("Invalid timestamp format. Use ISO 8601 format (e.g., 2024-01-20T10:00:00Z)")?;
        timestamp.to_string()
    } else {
        // Clear the not_before (make it ready now)
        task.not_before = None;
        save_graph(&graph, &path).context("Failed to save graph")?;
        println!("Cleared not_before for '{}' - task is now ready", id);
        return Ok(());
    };

    task.not_before = Some(new_timestamp.clone());
    save_graph(&graph, &path).context("Failed to save graph")?;

    println!("Rescheduled '{}' - not ready until {}", id, new_timestamp);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Status, Task, WorkGraph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
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
    fn test_reschedule_after_hours() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "Task 1");
        setup_workgraph(dir.path(), vec![task]);

        run(dir.path(), "t1", Some(24.0), None).unwrap();

        let graph = load_graph(&graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.not_before.is_some());

        // Verify the timestamp is roughly 24 hours in the future
        let not_before: DateTime<Utc> = task.not_before.as_ref().unwrap().parse().unwrap();
        let expected = Utc::now() + Duration::hours(24);
        let diff = (not_before - expected).num_seconds().abs();
        assert!(diff < 5); // Within 5 seconds
    }

    #[test]
    fn test_reschedule_at_timestamp() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "Task 1");
        setup_workgraph(dir.path(), vec![task]);

        run(dir.path(), "t1", None, Some("2099-06-15T10:00:00Z")).unwrap();

        let graph = load_graph(&graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.not_before, Some("2099-06-15T10:00:00Z".to_string()));
    }

    #[test]
    fn test_reschedule_clear() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Task 1");
        task.not_before = Some("2099-01-01T00:00:00Z".to_string());
        setup_workgraph(dir.path(), vec![task]);

        // Call with no duration or timestamp to clear
        run(dir.path(), "t1", None, None).unwrap();

        let graph = load_graph(&graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.not_before.is_none());
    }

    #[test]
    fn test_reschedule_nonexistent_task() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![]);

        let result = run(dir.path(), "nonexistent", Some(24.0), None);
        assert!(result.is_err());
    }

    #[test]
    fn test_reschedule_invalid_timestamp() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "Task 1");
        setup_workgraph(dir.path(), vec![task]);

        let result = run(dir.path(), "t1", None, Some("not-a-timestamp"));
        assert!(result.is_err());
    }

    #[test]
    fn test_reschedule_uninitialized_workgraph() {
        let dir = tempdir().unwrap();
        let result = run(dir.path(), "t1", Some(24.0), None);
        assert!(result.is_err());
    }
}
