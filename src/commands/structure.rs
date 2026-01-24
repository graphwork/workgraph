use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::graph::Status;
use workgraph::parser::load_graph;

use super::graph_path;

/// Information about a dead end task
struct DeadEndInfo {
    id: String,
    status: Status,
    is_final_deliverable: bool,
}

/// Run the structure analysis command
pub fn run(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Collect all tasks
    let tasks: Vec<_> = graph.tasks().collect();

    if tasks.is_empty() {
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "entry_points": [],
                    "dead_ends": [],
                    "high_impact_roots": []
                })
            );
        } else {
            println!("No tasks in graph.");
        }
        return Ok(());
    }

    // Find entry points: tasks with no blockers (empty blocked_by)
    let entry_points: Vec<&str> = tasks
        .iter()
        .filter(|t| t.blocked_by.is_empty())
        .map(|t| t.id.as_str())
        .collect();

    // Build reverse dependency map: for each task, which tasks depend on it
    // A task X is depended on by Y if Y.blocked_by contains X
    let mut dependents: HashMap<&str, HashSet<&str>> = HashMap::new();
    for task in &tasks {
        // Initialize empty set for every task
        dependents.entry(task.id.as_str()).or_default();
        // Add reverse edges
        for blocker_id in &task.blocked_by {
            dependents
                .entry(blocker_id.as_str())
                .or_default()
                .insert(task.id.as_str());
        }
    }

    // Find dead ends: tasks that no other task depends on
    let dead_ends: Vec<DeadEndInfo> = tasks
        .iter()
        .filter(|t| {
            dependents
                .get(t.id.as_str())
                .map(|d| d.is_empty())
                .unwrap_or(true)
        })
        .map(|t| {
            // Heuristic: if task is done or has "deploy", "release", "doc", "meeting", "final" in id/title,
            // it's likely an expected final deliverable
            let id_lower = t.id.to_lowercase();
            let title_lower = t.title.to_lowercase();
            let is_final = t.status == Status::Done
                || id_lower.contains("deploy")
                || id_lower.contains("release")
                || id_lower.contains("doc")
                || id_lower.contains("meeting")
                || id_lower.contains("final")
                || title_lower.contains("deploy")
                || title_lower.contains("release")
                || title_lower.contains("documentation")
                || title_lower.contains("meeting")
                || title_lower.contains("final");

            DeadEndInfo {
                id: t.id.clone(),
                status: t.status.clone(),
                is_final_deliverable: is_final,
            }
        })
        .collect();

    // Calculate transitive dependent counts for high-impact roots
    // Using memoization to avoid recalculating
    let mut transitive_counts: HashMap<&str, usize> = HashMap::new();
    for task in &tasks {
        calculate_transitive_dependents(task.id.as_str(), &dependents, &mut transitive_counts);
    }

    // Find high-impact roots: tasks blocking 5+ tasks transitively
    let mut high_impact: Vec<(&str, usize)> = transitive_counts
        .iter()
        .filter(|&(_, count)| *count >= 5)
        .map(|(&id, &count)| (id, count))
        .collect();

    // Sort by count descending
    high_impact.sort_by(|a, b| b.1.cmp(&a.1));

    if json {
        output_json(&entry_points, &dead_ends, &high_impact);
    } else {
        output_text(&entry_points, &dead_ends, &high_impact);
    }

    Ok(())
}

/// Calculate the number of tasks that transitively depend on a given task
fn calculate_transitive_dependents<'a>(
    task_id: &'a str,
    dependents: &HashMap<&'a str, HashSet<&'a str>>,
    cache: &mut HashMap<&'a str, usize>,
) -> usize {
    if let Some(&count) = cache.get(task_id) {
        return count;
    }

    let mut visited = HashSet::new();
    let mut stack = vec![task_id];
    let mut count = 0;

    while let Some(current) = stack.pop() {
        if let Some(deps) = dependents.get(current) {
            for &dep in deps {
                if visited.insert(dep) {
                    count += 1;
                    stack.push(dep);
                }
            }
        }
    }

    cache.insert(task_id, count);
    count
}

fn output_json(entry_points: &[&str], dead_ends: &[DeadEndInfo], high_impact: &[(&str, usize)]) {
    let entry_points_json: Vec<_> = entry_points.iter().map(|&id| serde_json::json!(id)).collect();

    let dead_ends_json: Vec<_> = dead_ends
        .iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "status": d.status,
                "expected": d.is_final_deliverable,
                "warning": !d.is_final_deliverable && d.status == Status::Open
            })
        })
        .collect();

    let high_impact_json: Vec<_> = high_impact
        .iter()
        .map(|(id, count)| {
            serde_json::json!({
                "id": id,
                "transitive_dependents": count
            })
        })
        .collect();

    let output = serde_json::json!({
        "entry_points": entry_points_json,
        "dead_ends": dead_ends_json,
        "high_impact_roots": high_impact_json
    });

    println!("{}", serde_json::to_string_pretty(&output).unwrap());
}

fn output_text(entry_points: &[&str], dead_ends: &[DeadEndInfo], high_impact: &[(&str, usize)]) {
    // Entry points
    println!("Entry points ({} tasks):", entry_points.len());
    if entry_points.is_empty() {
        println!("  (none)");
    } else {
        let entry_list = entry_points.join(", ");
        println!("  {}", entry_list);
    }
    println!();

    // Dead ends
    println!("Dead ends ({} tasks):", dead_ends.len());
    if dead_ends.is_empty() {
        println!("  (none)");
    } else {
        for d in dead_ends {
            if d.is_final_deliverable {
                println!("  {} (expected - final deliverable)", d.id);
            } else if d.status == Status::Open {
                println!("  {} (WARNING: no dependents, status=open)", d.id);
            } else {
                println!("  {} (status={:?})", d.id, d.status);
            }
        }
    }
    println!();

    // High-impact roots
    println!("High-impact roots (blocking 5+ tasks transitively):");
    if high_impact.is_empty() {
        println!("  (none)");
    } else {
        for (id, count) in high_impact {
            println!("  {}: {} tasks depend on this", id, count);
        }
    }
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

    fn setup_test_graph() -> (TempDir, std::path::PathBuf) {
        let tmp = TempDir::new().unwrap();
        let workgraph_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&workgraph_dir).unwrap();
        let graph_file = workgraph_dir.join("graph.jsonl");
        (tmp, graph_file)
    }

    #[test]
    fn test_entry_points_simple() {
        let (_tmp, graph_file) = setup_test_graph();

        let mut graph = WorkGraph::new();

        // t1 is an entry point (no blockers)
        let t1 = make_task("t1", "Task 1");

        // t2 depends on t1
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        save_graph(&graph, &graph_file).unwrap();

        // Run the command
        let result = run(graph_file.parent().unwrap(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_dead_ends_detection() {
        let (_tmp, graph_file) = setup_test_graph();

        let mut graph = WorkGraph::new();

        // t1 blocks t2
        let t1 = make_task("t1", "Task 1");

        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];

        // t2 is a dead end (nothing depends on it)
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        save_graph(&graph, &graph_file).unwrap();

        let result = run(graph_file.parent().unwrap(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_transitive_dependents() {
        // Test the transitive calculation
        // t1 -> t2 -> t3 -> t4 -> t5 -> t6
        // t1 should have 5 transitive dependents

        let mut dependents: HashMap<&str, HashSet<&str>> = HashMap::new();
        dependents.insert("t1", ["t2"].into_iter().collect());
        dependents.insert("t2", ["t3"].into_iter().collect());
        dependents.insert("t3", ["t4"].into_iter().collect());
        dependents.insert("t4", ["t5"].into_iter().collect());
        dependents.insert("t5", ["t6"].into_iter().collect());
        dependents.insert("t6", HashSet::new());

        let mut cache = HashMap::new();
        let count = calculate_transitive_dependents("t1", &dependents, &mut cache);
        assert_eq!(count, 5); // t2, t3, t4, t5, t6
    }

    #[test]
    fn test_transitive_dependents_diamond() {
        // Diamond pattern: t1 -> t2, t3 -> t4
        // t1 should have 3 transitive dependents (t2, t3, t4)

        let mut dependents: HashMap<&str, HashSet<&str>> = HashMap::new();
        dependents.insert("t1", ["t2", "t3"].into_iter().collect());
        dependents.insert("t2", ["t4"].into_iter().collect());
        dependents.insert("t3", ["t4"].into_iter().collect());
        dependents.insert("t4", HashSet::new());

        let mut cache = HashMap::new();
        let count = calculate_transitive_dependents("t1", &dependents, &mut cache);
        assert_eq!(count, 3); // t2, t3, t4 (each counted once)
    }

    #[test]
    fn test_empty_graph() {
        let (_tmp, graph_file) = setup_test_graph();

        let graph = WorkGraph::new();
        save_graph(&graph, &graph_file).unwrap();

        let result = run(graph_file.parent().unwrap(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_json_output() {
        let (_tmp, graph_file) = setup_test_graph();

        let mut graph = WorkGraph::new();
        let t1 = make_task("api-design", "Design API");
        graph.add_node(Node::Task(t1));

        save_graph(&graph, &graph_file).unwrap();

        let result = run(graph_file.parent().unwrap(), true);
        assert!(result.is_ok());
    }
}
