use std::path::Path;

use anyhow::{Context, Result};
use workgraph::graph::TokenUsage;
use workgraph::parser::save_graph;

use super::load_workgraph_mut;

/// Set or accumulate token usage on a task from a JSON string.
///
/// If the task already has token_usage, the new values are accumulated
/// (component-wise addition). Otherwise the new values are set directly.
pub fn run(dir: &Path, task_id: &str, json: &str) -> Result<()> {
    let usage: TokenUsage =
        serde_json::from_str(json).context("Failed to parse token usage JSON")?;

    let (mut graph, graph_path) = load_workgraph_mut(dir)?;
    let task = graph.get_task_mut_or_err(task_id)?;

    if let Some(ref mut existing) = task.token_usage {
        existing.accumulate(&usage);
    } else {
        task.token_usage = Some(usage);
    }

    save_graph(&graph, &graph_path).context("Failed to save graph after setting token usage")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};

    fn setup_dir_with_task(task_id: &str) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let gp = dir.join("graph.jsonl");
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: task_id.to_string(),
            title: "Test task".to_string(),
            ..Task::default()
        }));
        save_graph(&graph, &gp).unwrap();
        tmp
    }

    #[test]
    fn test_tokens_set_on_empty() {
        let tmp = setup_dir_with_task("t1");
        let json = r#"{"cost_usd":0.5,"input_tokens":100,"output_tokens":50}"#;
        run(tmp.path(), "t1", json).unwrap();

        let (graph, _) = load_workgraph_mut(tmp.path()).unwrap();
        let task = graph.get_task("t1").unwrap();
        let usage = task.token_usage.as_ref().unwrap();
        assert!((usage.cost_usd - 0.5).abs() < f64::EPSILON);
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
    }

    #[test]
    fn test_tokens_accumulate() {
        let tmp = setup_dir_with_task("t2");
        let json1 = r#"{"cost_usd":0.3,"input_tokens":100,"output_tokens":50}"#;
        let json2 = r#"{"cost_usd":0.2,"input_tokens":200,"output_tokens":80}"#;
        run(tmp.path(), "t2", json1).unwrap();
        run(tmp.path(), "t2", json2).unwrap();

        let (graph, _) = load_workgraph_mut(tmp.path()).unwrap();
        let task = graph.get_task("t2").unwrap();
        let usage = task.token_usage.as_ref().unwrap();
        assert!((usage.cost_usd - 0.5).abs() < f64::EPSILON);
        assert_eq!(usage.input_tokens, 300);
        assert_eq!(usage.output_tokens, 130);
    }

    #[test]
    fn test_tokens_invalid_json() {
        let tmp = setup_dir_with_task("t3");
        let result = run(tmp.path(), "t3", "not json");
        assert!(result.is_err());
    }

    #[test]
    fn test_tokens_missing_task() {
        let tmp = setup_dir_with_task("t4");
        let result = run(tmp.path(), "nonexistent", r#"{"input_tokens":1}"#);
        assert!(result.is_err());
    }
}
