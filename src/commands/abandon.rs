use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::save_graph;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(dir: &Path, id: &str, reason: Option<&str>) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    let task = graph.get_task_mut_or_err(id)?;

    if task.status == Status::Done {
        anyhow::bail!("Task '{}' is already done and cannot be abandoned", id);
    }

    if task.status == Status::Abandoned {
        println!("Task '{}' is already abandoned", id);
        return Ok(());
    }

    let prev_assigned = task.assigned.clone();
    task.status = Status::Abandoned;
    task.failure_reason = reason.map(String::from);

    let log_message = match reason {
        Some(r) => format!("Task abandoned: {}", r),
        None => "Task abandoned".to_string(),
    };
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: task.assigned.clone(),
        message: log_message,
    });

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "abandon",
        Some(id),
        prev_assigned.as_deref(),
        serde_json::json!({ "reason": reason, "prev_assigned": prev_assigned }),
        config.log.rotation_threshold,
    );

    let reason_msg = reason.map(|r| format!(" ({})", r)).unwrap_or_default();
    println!("Marked '{}' as abandoned{}", id, reason_msg);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Status, Task, WorkGraph};
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    fn setup_graph(dir: &Path, graph: &WorkGraph) {
        std::fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        save_graph(graph, &path).unwrap();
    }

    #[test]
    fn test_abandon_open_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Open task")));
        setup_graph(&dir, &graph);

        let result = run(&dir, "t1", Some("no longer needed"));
        assert!(result.is_ok());

        let reloaded = load_graph(graph_path(&dir)).unwrap();
        let task = reloaded.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Abandoned);
        assert_eq!(task.failure_reason.as_deref(), Some("no longer needed"));
        assert!(!task.log.is_empty());
        assert!(
            task.log
                .last()
                .unwrap()
                .message
                .contains("Task abandoned: no longer needed")
        );
    }

    #[test]
    fn test_abandon_in_progress_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        let mut t = make_task("t1", "Active task");
        t.status = Status::InProgress;
        t.assigned = Some("agent-1".to_string());
        graph.add_node(Node::Task(t));
        setup_graph(&dir, &graph);

        let result = run(&dir, "t1", None);
        assert!(result.is_ok());

        let reloaded = load_graph(graph_path(&dir)).unwrap();
        let task = reloaded.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Abandoned);
        assert!(task.failure_reason.is_none());
        assert!(task.log.last().unwrap().message == "Task abandoned");
    }

    #[test]
    fn test_abandon_failed_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        let mut t = make_task("t1", "Failed task");
        t.status = Status::Failed;
        graph.add_node(Node::Task(t));
        setup_graph(&dir, &graph);

        let result = run(&dir, "t1", Some("giving up"));
        assert!(result.is_ok());

        let reloaded = load_graph(graph_path(&dir)).unwrap();
        let task = reloaded.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Abandoned);
        assert_eq!(task.failure_reason.as_deref(), Some("giving up"));
    }

    #[test]
    fn test_abandon_done_task_errors() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        let mut t = make_task("t1", "Done task");
        t.status = Status::Done;
        graph.add_node(Node::Task(t));
        setup_graph(&dir, &graph);

        let result = run(&dir, "t1", None);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("already done"));
    }

    #[test]
    fn test_abandon_already_abandoned_is_noop() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        let mut t = make_task("t1", "Abandoned task");
        t.status = Status::Abandoned;
        graph.add_node(Node::Task(t));
        setup_graph(&dir, &graph);

        let result = run(&dir, "t1", None);
        assert!(result.is_ok());

        // Should not add another log entry
        let reloaded = load_graph(graph_path(&dir)).unwrap();
        let task = reloaded.get_task("t1").unwrap();
        assert!(task.log.is_empty());
    }

    #[test]
    fn test_abandon_nonexistent_task_errors() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Exists")));
        setup_graph(&dir, &graph);

        let result = run(&dir, "nope", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_abandon_not_initialized() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let result = run(&dir, "t1", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[test]
    fn test_abandon_reason_stored_in_log() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(".workgraph");

        let mut graph = WorkGraph::new();
        let mut t = make_task("t1", "Task with reason");
        t.assigned = Some("worker-42".to_string());
        graph.add_node(Node::Task(t));
        setup_graph(&dir, &graph);

        run(&dir, "t1", Some("requirements changed")).unwrap();

        let reloaded = load_graph(graph_path(&dir)).unwrap();
        let task = reloaded.get_task("t1").unwrap();
        let last_log = task.log.last().unwrap();
        assert!(last_log.message.contains("requirements changed"));
        assert_eq!(last_log.actor.as_deref(), Some("worker-42"));
    }
}
