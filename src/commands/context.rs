use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use workgraph::graph::Status;
use workgraph::parser::load_graph;

use super::graph_path;

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
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Collect all artifacts from dependencies (blocked_by)
    let mut available_context = Vec::new();
    let mut all_artifacts: HashSet<String> = HashSet::new();

    for dep_id in &task.blocked_by {
        if let Some(dep_task) = graph.get_task(dep_id) {
            for artifact in &dep_task.artifacts {
                all_artifacts.insert(artifact.clone());
            }
            if !dep_task.artifacts.is_empty() {
                available_context.push(ContextSource {
                    task_id: dep_task.id.clone(),
                    task_title: dep_task.title.clone(),
                    status: dep_task.status.clone(),
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
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Find tasks that list this task in their blocked_by
    let mut dependents: Vec<(String, String, Vec<String>)> = Vec::new();

    for other in graph.tasks() {
        if other.blocked_by.contains(&task_id.to_string()) {
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
                let produced = if task.artifacts.contains(d) { " [produced]" } else { " [expected]" };
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
        t2.blocked_by = vec!["t1".to_string()];
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
        t2.blocked_by = vec!["t1".to_string()];
        t2.inputs = vec!["missing.txt".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        save_graph(&graph, &path).unwrap();

        // Should work but show missing inputs
        let result = run(temp_dir.path(), "t2", false);
        assert!(result.is_ok());
    }
}
