use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::LogEntry;
use workgraph::parser::save_graph;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(dir: &Path, id: &str) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    let task = graph.get_task_mut_or_err(id)?;

    if task.paused {
        anyhow::bail!("Task '{}' is already paused", id);
    }

    task.paused = true;
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: None,
        message: "Task paused".to_string(),
    });

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "pause",
        Some(id),
        None,
        serde_json::json!({}),
        config.log.rotation_threshold,
    );

    println!("Paused '{}'", id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Status, Task, WorkGraph};

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
    fn test_pause_open_task() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![make_task("t1", "Test", Status::Open)]);

        let result = run(dir.path(), "t1");
        assert!(result.is_ok());

        let graph = load_graph(&graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.paused);
        assert_eq!(task.status, Status::Open);
    }

    #[test]
    fn test_pause_already_paused_fails() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        setup_workgraph(dir.path(), vec![task]);

        let result = run(dir.path(), "t1");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already paused"));
    }

    #[test]
    fn test_pause_nonexistent_task_fails() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![]);

        let result = run(dir.path(), "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_pause_adds_log_entry() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![make_task("t1", "Test", Status::Open)]);

        run(dir.path(), "t1").unwrap();

        let graph = load_graph(&graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.log.len(), 1);
        assert!(task.log[0].message.contains("paused"));
    }
}
