use anyhow::Result;
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use workgraph::graph::Status;

/// Source of context (which dependency produced it)
#[derive(Debug, Serialize)]
struct ContextSource {
    task_id: String,
    task_title: String,
    status: Status,
    artifacts: Vec<String>,
}

/// Context available for a task
#[derive(Debug, Serialize)]
struct TaskContext {
    task_id: String,
    task_title: String,
    declared_inputs: Vec<String>,
    available_context: Vec<ContextSource>,
    missing_inputs: Vec<String>,
}

/// Show available context for a task from its dependencies
pub fn run(dir: &Path, task_id: &str, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    let task = graph.get_task_or_err(task_id)?;

    // Collect all artifacts from dependencies (after)
    let mut available_context = Vec::new();
    let mut all_artifacts: HashSet<String> = HashSet::new();

    for dep_id in &task.after {
        if let Some(dep_task) = graph.get_task(dep_id) {
            for artifact in &dep_task.artifacts {
                all_artifacts.insert(artifact.clone());
            }
            if !dep_task.artifacts.is_empty() {
                available_context.push(ContextSource {
                    task_id: dep_task.id.clone(),
                    task_title: dep_task.title.clone(),
                    status: dep_task.status,
                    artifacts: dep_task.artifacts.clone(),
                });
            }
        }
    }

    // Find missing inputs (declared but not available from dependencies)
    let missing_inputs: Vec<String> = task
        .inputs
        .iter()
        .filter(|input| !all_artifacts.contains(*input))
        .cloned()
        .collect();

    let context = TaskContext {
        task_id: task.id.clone(),
        task_title: task.title.clone(),
        declared_inputs: task.inputs.clone(),
        available_context,
        missing_inputs,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&context)?);
    } else {
        println!("Context for: {} - {}", context.task_id, context.task_title);
        println!();

        if !context.declared_inputs.is_empty() {
            println!("Declared inputs:");
            for input in &context.declared_inputs {
                let status = if all_artifacts.contains(input) {
                    " [available]"
                } else {
                    " [missing]"
                };
                println!("  {}{}", input, status);
            }
            println!();
        }

        if !context.available_context.is_empty() {
            println!("Available from dependencies:");
            for source in &context.available_context {
                let status_str = match source.status {
                    Status::Done => "[done]",
                    Status::InProgress => "[in-progress]",
                    _ => "[not done]",
                };
                println!("  {} {} {}", source.task_id, status_str, source.task_title);
                for artifact in &source.artifacts {
                    println!("    - {}", artifact);
                }
            }
        } else {
            println!("No artifacts available from dependencies yet.");
        }

        if !context.missing_inputs.is_empty() {
            println!();
            println!("Missing inputs (not produced by dependencies):");
            for input in &context.missing_inputs {
                println!("  {}", input);
            }
        }
    }

    Ok(())
}

/// Show what tasks depend on a task's outputs (reverse context query)
pub fn run_dependents(dir: &Path, task_id: &str, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    let task = graph.get_task_or_err(task_id)?;

    // Find tasks that list this task in their after
    let mut dependents: Vec<(String, String, Vec<String>)> = Vec::new();

    for other in graph.tasks() {
        if other.after.contains(&task_id.to_string()) {
            // Check which of our artifacts this task needs
            let needed: Vec<String> = other
                .inputs
                .iter()
                .filter(|input| task.artifacts.contains(input) || task.deliverables.contains(input))
                .cloned()
                .collect();

            dependents.push((other.id.clone(), other.title.clone(), needed));
        }
    }

    if json {
        let output = serde_json::json!({
            "task_id": task_id,
            "artifacts": task.artifacts,
            "deliverables": task.deliverables,
            "dependents": dependents.iter().map(|(id, title, needed)| {
                serde_json::json!({
                    "task_id": id,
                    "title": title,
                    "needs": needed,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Task: {} - {}", task.id, task.title);
        println!();

        if !task.artifacts.is_empty() || !task.deliverables.is_empty() {
            println!("Outputs:");
            for d in &task.deliverables {
                let produced = if task.artifacts.contains(d) {
                    " [produced]"
                } else {
                    " [expected]"
                };
                println!("  {}{}", d, produced);
            }
            for a in &task.artifacts {
                if !task.deliverables.contains(a) {
                    println!("  {} [extra]", a);
                }
            }
            println!();
        }

        if !dependents.is_empty() {
            println!("Tasks depending on outputs:");
            for (id, title, needed) in &dependents {
                println!("  {} - {}", id, title);
                if !needed.is_empty() {
                    for n in needed {
                        println!("    needs: {}", n);
                    }
                }
            }
        } else {
            println!("No tasks depend on this task's outputs.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    fn setup_dependency_chain() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        // Task 1 produces output.txt
        let mut t1 = make_task("t1", "Producer Task");
        t1.status = Status::Done;
        t1.deliverables = vec!["output.txt".to_string()];
        t1.artifacts = vec!["output.txt".to_string()];

        // Task 2 depends on t1 and needs output.txt
        let mut t2 = make_task("t2", "Consumer Task");
        t2.after = vec!["t1".to_string()];
        t2.inputs = vec!["output.txt".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        save_graph(&graph, &path).unwrap();

        temp_dir
    }

    #[test]
    fn test_context_shows_available_artifacts() {
        let temp_dir = setup_dependency_chain();

        let result = run(temp_dir.path(), "t2", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_context_json_output() {
        let temp_dir = setup_dependency_chain();

        let result = run(temp_dir.path(), "t2", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_dependents_shows_consumers() {
        let temp_dir = setup_dependency_chain();

        let result = run_dependents(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_missing_inputs() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Producer");
        t1.status = Status::Done;
        // No artifacts produced

        let mut t2 = make_task("t2", "Consumer");
        t2.after = vec!["t1".to_string()];
        t2.inputs = vec!["missing.txt".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        save_graph(&graph, &path).unwrap();

        // Should work but show missing inputs
        let result = run(temp_dir.path(), "t2", false);
        assert!(result.is_ok());
    }

    // --- Deep dependency tree tests ---

    #[test]
    fn test_context_deep_dependency_chain() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        // Chain: t1 -> t2 -> t3 -> t4
        // t4 should only see context from its direct dependency (t3)
        let mut t1 = make_task("t1", "Root producer");
        t1.status = Status::Done;
        t1.artifacts = vec!["root-output.txt".to_string()];

        let mut t2 = make_task("t2", "Middle 1");
        t2.status = Status::Done;
        t2.after = vec!["t1".to_string()];
        t2.artifacts = vec!["middle-output.txt".to_string()];

        let mut t3 = make_task("t3", "Middle 2");
        t3.status = Status::Done;
        t3.after = vec!["t2".to_string()];
        t3.artifacts = vec!["final-input.txt".to_string()];

        let mut t4 = make_task("t4", "Consumer");
        t4.after = vec!["t3".to_string()];
        t4.inputs = vec!["final-input.txt".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        graph.add_node(Node::Task(t4));
        save_graph(&graph, &path).unwrap();

        // t4 only directly depends on t3, so only t3's artifacts are shown
        let result = run(temp_dir.path(), "t4", false);
        assert!(result.is_ok());

        // JSON output should confirm
        let result = run(temp_dir.path(), "t4", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_context_task_with_no_dependencies() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Independent task");
        graph.add_node(Node::Task(t1));
        save_graph(&graph, &path).unwrap();

        // Task with no dependencies should have no context
        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_context_task_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "nonexistent", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_context_multiple_dependency_artifacts() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        // t3 depends on both t1 and t2
        let mut t1 = make_task("t1", "Producer A");
        t1.status = Status::Done;
        t1.artifacts = vec!["a.txt".to_string(), "b.txt".to_string()];

        let mut t2 = make_task("t2", "Producer B");
        t2.status = Status::Done;
        t2.artifacts = vec!["c.txt".to_string()];

        let mut t3 = make_task("t3", "Consumer");
        t3.after = vec!["t1".to_string(), "t2".to_string()];
        t3.inputs = vec![
            "a.txt".to_string(),
            "c.txt".to_string(),
            "d.txt".to_string(),
        ];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        save_graph(&graph, &path).unwrap();

        // a.txt and c.txt are available, d.txt is missing
        let result = run(temp_dir.path(), "t3", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_context_dependency_with_no_artifacts() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "No artifacts");
        t1.status = Status::Done;
        // No artifacts

        let mut t2 = make_task("t2", "Consumer");
        t2.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t2", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_context_json_output_structure() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Producer");
        t1.status = Status::Done;
        t1.artifacts = vec!["output.txt".to_string()];

        let mut t2 = make_task("t2", "Consumer");
        t2.after = vec!["t1".to_string()];
        t2.inputs = vec!["output.txt".to_string(), "missing.txt".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        save_graph(&graph, &path).unwrap();

        // JSON output should work
        let result = run(temp_dir.path(), "t2", true);
        assert!(result.is_ok());
    }

    // --- run_dependents tests ---

    #[test]
    fn test_dependents_task_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let result = run_dependents(temp_dir.path(), "nonexistent", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_dependents_no_dependents() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Leaf task");
        graph.add_node(Node::Task(t1));
        save_graph(&graph, &path).unwrap();

        let result = run_dependents(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_dependents_multiple_consumers() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Producer");
        t1.artifacts = vec!["shared.txt".to_string()];
        t1.deliverables = vec!["shared.txt".to_string()];

        let mut t2 = make_task("t2", "Consumer 1");
        t2.after = vec!["t1".to_string()];
        t2.inputs = vec!["shared.txt".to_string()];

        let mut t3 = make_task("t3", "Consumer 2");
        t3.after = vec!["t1".to_string()];
        t3.inputs = vec!["shared.txt".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        save_graph(&graph, &path).unwrap();

        let result = run_dependents(temp_dir.path(), "t1", false);
        assert!(result.is_ok());

        // JSON format
        let result = run_dependents(temp_dir.path(), "t1", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_dependents_json_output() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Producer");
        t1.artifacts = vec!["data.json".to_string()];
        t1.deliverables = vec!["data.json".to_string()];

        let mut t2 = make_task("t2", "Consumer");
        t2.after = vec!["t1".to_string()];
        t2.inputs = vec!["data.json".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        save_graph(&graph, &path).unwrap();

        let result = run_dependents(temp_dir.path(), "t1", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_context_no_workgraph() {
        let temp_dir = TempDir::new().unwrap();
        // Don't create graph.jsonl

        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_dependents_no_workgraph() {
        let temp_dir = TempDir::new().unwrap();

        let result = run_dependents(temp_dir.path(), "t1", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_context_dependency_not_in_graph() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        // t1 depends on a task that doesn't exist in the graph
        let mut t1 = make_task("t1", "Has missing dependency");
        t1.after = vec!["nonexistent".to_string()];
        t1.inputs = vec!["file.txt".to_string()];

        graph.add_node(Node::Task(t1));
        save_graph(&graph, &path).unwrap();

        // Should handle gracefully — the nonexistent dependency is skipped
        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_context_empty_graph() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // Task not found in empty graph
        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_dependents_deliverables_matching() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        // t1 has deliverables (expected) and artifacts (produced)
        let mut t1 = make_task("t1", "Producer");
        t1.deliverables = vec!["expected.txt".to_string()];
        t1.artifacts = vec!["extra.txt".to_string()];

        let mut t2 = make_task("t2", "Consumer");
        t2.after = vec!["t1".to_string()];
        t2.inputs = vec!["expected.txt".to_string(), "extra.txt".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        save_graph(&graph, &path).unwrap();

        // Both deliverables and artifacts should be checked for matching
        let result = run_dependents(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }
}
