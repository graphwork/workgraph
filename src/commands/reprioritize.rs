use anyhow::{Context, Result};
use std::path::Path;
use workgraph::graph::{PRIORITY_DEFAULT, PRIORITY_CRITICAL, PRIORITY_HIGH, PRIORITY_IDLE, PRIORITY_LOW};
use workgraph::parser::modify_graph;

use super::add::parse_priority;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(dir: &Path, id: &str, priority: &str) -> Result<()> {
    let path = super::graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let new_priority = parse_priority(Some(priority));

    let mut error: Option<anyhow::Error> = None;
    let mut old_priority = PRIORITY_DEFAULT;
    modify_graph(&path, |graph| {
        let task = match graph.get_task_mut(id) {
            Some(t) => t,
            None => {
                error = Some(anyhow::anyhow!("Task '{}' not found", id));
                return false;
            }
        };

        old_priority = task.priority;
        if task.priority == new_priority {
            return false;
        }

        task.priority = new_priority;
        true
    })
    .context("Failed to modify graph")?;
    if let Some(e) = error {
        return Err(e);
    }

    super::notify_graph_changed(dir);

    if old_priority == new_priority {
        println!("Task '{}' already has priority '{}'", id, new_priority);
    } else {
        println!(
            "Reprioritized '{}': {} -> {}",
            id, old_priority, new_priority
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
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
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
    fn test_reprioritize_normal_to_high() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "Task 1");
        setup_workgraph(dir.path(), vec![task]);

        run(dir.path(), "t1", "high").unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.priority, PRIORITY_HIGH);
    }

    #[test]
    fn test_reprioritize_to_critical() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "Task 1");
        setup_workgraph(dir.path(), vec![task]);

        run(dir.path(), "t1", "critical").unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.priority, PRIORITY_CRITICAL);
    }

    #[test]
    fn test_reprioritize_to_low() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "Task 1");
        setup_workgraph(dir.path(), vec![task]);

        run(dir.path(), "t1", "low").unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.priority, PRIORITY_LOW);
    }

    #[test]
    fn test_reprioritize_to_idle() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "Task 1");
        setup_workgraph(dir.path(), vec![task]);

        run(dir.path(), "t1", "idle").unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.priority, PRIORITY_IDLE);
    }

    #[test]
    fn test_reprioritize_same_priority_noop() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "Task 1");
        setup_workgraph(dir.path(), vec![task]);

        run(dir.path(), "t1", "normal").unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.priority, PRIORITY_DEFAULT);
    }

    #[test]
    fn test_reprioritize_nonexistent_task() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![]);

        let result = run(dir.path(), "nonexistent", "high");
        assert!(result.is_err());
    }

    #[test]
    fn test_reprioritize_uninitialized_workgraph() {
        let dir = tempdir().unwrap();
        let result = run(dir.path(), "t1", "high");
        assert!(result.is_err());
    }

    #[test]
    fn test_reprioritize_case_insensitive() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "Task 1");
        setup_workgraph(dir.path(), vec![task]);

        run(dir.path(), "t1", "HIGH").unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.priority, PRIORITY_HIGH);
    }

    #[test]
    fn test_reprioritize_with_existing_priority() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Task 1");
        task.priority = PRIORITY_CRITICAL;
        setup_workgraph(dir.path(), vec![task]);

        run(dir.path(), "t1", "low").unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.priority, PRIORITY_LOW);
    }

    #[test]
    fn test_reprioritize_accepts_numeric_and_named() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "Task 1");
        setup_workgraph(dir.path(), vec![task]);

        // Numeric
        run(dir.path(), "t1", "42").unwrap();
        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.priority, 42);

        // Named alias
        run(dir.path(), "t1", "high").unwrap();
        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.priority, PRIORITY_HIGH);
    }
}
