use anyhow::{Context, Result};
use std::path::Path;
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

/// Register an artifact (produced output) for a task
pub fn run_add(dir: &Path, task_id: &str, artifact_path: &str) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Check if artifact already registered
    if task.artifacts.contains(&artifact_path.to_string()) {
        println!("Artifact '{}' already registered for task '{}'", artifact_path, task_id);
        return Ok(());
    }

    task.artifacts.push(artifact_path.to_string());

    save_graph(&graph, &path).context("Failed to save graph")?;

    println!("Registered artifact '{}' for task '{}'", artifact_path, task_id);
    Ok(())
}

/// Remove an artifact from a task
pub fn run_remove(dir: &Path, task_id: &str, artifact_path: &str) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    let original_len = task.artifacts.len();
    task.artifacts.retain(|a| a != artifact_path);

    if task.artifacts.len() == original_len {
        anyhow::bail!("Artifact '{}' not found on task '{}'", artifact_path, task_id);
    }

    save_graph(&graph, &path).context("Failed to save graph")?;

    println!("Removed artifact '{}' from task '{}'", artifact_path, task_id);
    Ok(())
}

/// List artifacts for a task
pub fn run_list(dir: &Path, task_id: &str, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    if json {
        let output = serde_json::json!({
            "task_id": task_id,
            "deliverables": task.deliverables,
            "artifacts": task.artifacts,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Task: {} - {}", task.id, task.title);
        println!();

        if !task.deliverables.is_empty() {
            println!("Expected deliverables:");
            for d in &task.deliverables {
                let produced = if task.artifacts.contains(d) { " [produced]" } else { "" };
                println!("  {}{}", d, produced);
            }
            println!();
        }

        if !task.artifacts.is_empty() {
            println!("Produced artifacts:");
            for a in &task.artifacts {
                let expected = if task.deliverables.contains(a) { "" } else { " [extra]" };
                println!("  {}{}", a, expected);
            }
        } else {
            println!("No artifacts produced yet.");
        }
    }

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

    fn setup_graph() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Test Task");
        task.deliverables = vec!["output.txt".to_string()];
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        temp_dir
    }

    #[test]
    fn test_add_artifact() {
        let temp_dir = setup_graph();

        let result = run_add(temp_dir.path(), "t1", "output.txt");
        assert!(result.is_ok());

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.artifacts.contains(&"output.txt".to_string()));
    }

    #[test]
    fn test_add_artifact_duplicate() {
        let temp_dir = setup_graph();

        run_add(temp_dir.path(), "t1", "output.txt").unwrap();
        let result = run_add(temp_dir.path(), "t1", "output.txt");
        assert!(result.is_ok()); // Should succeed but not duplicate

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.artifacts.len(), 1);
    }

    #[test]
    fn test_remove_artifact() {
        let temp_dir = setup_graph();

        run_add(temp_dir.path(), "t1", "output.txt").unwrap();
        let result = run_remove(temp_dir.path(), "t1", "output.txt");
        assert!(result.is_ok());

        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(task.artifacts.is_empty());
    }

    #[test]
    fn test_remove_nonexistent_artifact() {
        let temp_dir = setup_graph();

        let result = run_remove(temp_dir.path(), "t1", "nonexistent.txt");
        assert!(result.is_err());
    }

    #[test]
    fn test_list_artifacts() {
        let temp_dir = setup_graph();

        run_add(temp_dir.path(), "t1", "output.txt").unwrap();
        let result = run_list(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }
}
