use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::graph::{Status, WorkGraph};
use workgraph::parser::load_graph;

use super::graph_path;

/// Information about a bottleneck task
#[derive(Debug, Serialize)]
struct BottleneckInfo {
    id: String,
    title: String,
    direct_blocks: usize,
    transitive_blocks: usize,
    status: Status,
    assigned: Option<String>,
    recommendation: Option<String>,
}

/// JSON output structure
#[derive(Debug, Serialize)]
struct BottlenecksOutput {
    bottlenecks: Vec<BottleneckInfo>,
    total_tasks: usize,
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Build reverse index: task_id -> list of tasks that depend on it
    let reverse_index = build_reverse_index(&graph);

    // Calculate impact for each task
    let total_tasks = graph.tasks().count();
    let mut bottlenecks: Vec<BottleneckInfo> = Vec::new();

    for task in graph.tasks() {
        // Count direct dependents
        let direct_blocks = reverse_index
            .get(&task.id)
            .map(|v| v.len())
            .unwrap_or(0);

        // Count transitive dependents
        let mut transitive: HashSet<String> = HashSet::new();
        collect_transitive_dependents(&reverse_index, &task.id, &mut transitive);
        let transitive_blocks = transitive.len();

        // Only include tasks that block at least one other task
        if transitive_blocks > 0 {
            let recommendation = generate_recommendation(&task.status, transitive_blocks, total_tasks);

            bottlenecks.push(BottleneckInfo {
                id: task.id.clone(),
                title: task.title.clone(),
                direct_blocks,
                transitive_blocks,
                status: task.status.clone(),
                assigned: task.assigned.clone(),
                recommendation,
            });
        }
    }

    // Sort by transitive impact (highest first)
    bottlenecks.sort_by(|a, b| b.transitive_blocks.cmp(&a.transitive_blocks));

    // Take top 10
    let top_bottlenecks: Vec<BottleneckInfo> = bottlenecks.into_iter().take(10).collect();

    if json {
        let output = BottlenecksOutput {
            bottlenecks: top_bottlenecks,
            total_tasks,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if top_bottlenecks.is_empty() {
            println!("No bottlenecks found - no tasks are blocking other tasks.");
            return Ok(());
        }

        println!("Top bottlenecks by transitive impact:\n");

        for (i, bottleneck) in top_bottlenecks.iter().enumerate() {
            println!("{}. {}", i + 1, bottleneck.id);
            println!("   Directly blocks: {} tasks", bottleneck.direct_blocks);
            println!("   Transitively blocks: {} tasks", bottleneck.transitive_blocks);

            let status_str = match bottleneck.status {
                Status::Open => "OPEN (not started!)".to_string(),
                Status::InProgress => "in-progress".to_string(),
                Status::Done => "done (no longer blocking)".to_string(),
                Status::Blocked => "blocked".to_string(),
                Status::Failed => "FAILED (needs retry!)".to_string(),
                Status::Abandoned => "abandoned".to_string(),
            };
            print!("   Status: {}", status_str);

            if let Some(ref assigned) = bottleneck.assigned {
                println!("\n   Assigned: @{}", assigned);
            } else {
                println!();
            }

            if let Some(ref rec) = bottleneck.recommendation {
                println!("   RECOMMENDATION: {}", rec);
            }

            println!();
        }
    }

    Ok(())
}

/// Generate a recommendation based on status and impact
fn generate_recommendation(status: &Status, transitive_blocks: usize, total_tasks: usize) -> Option<String> {
    if total_tasks == 0 {
        return None;
    }

    let percentage = (transitive_blocks as f64 / total_tasks as f64 * 100.0).round() as usize;

    match status {
        Status::Done => None, // No recommendation for done tasks
        Status::Open if percentage >= 20 => {
            Some(format!("High priority - blocking {}% of project", percentage))
        }
        Status::Open if percentage >= 10 => {
            Some(format!("Medium priority - blocking {}% of project", percentage))
        }
        Status::InProgress if percentage >= 20 => {
            Some(format!("Critical path - blocking {}% of project", percentage))
        }
        Status::Blocked if percentage >= 20 => {
            Some(format!("Urgent: unblock this first - blocking {}% of project", percentage))
        }
        Status::Blocked if percentage >= 10 => {
            Some(format!("Priority: unblock this - blocking {}% of project", percentage))
        }
        _ => None,
    }
}

/// Build a reverse index: for each task, find what tasks list it in their `blocked_by`
fn build_reverse_index(graph: &WorkGraph) -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();

    for task in graph.tasks() {
        for blocker_id in &task.blocked_by {
            index
                .entry(blocker_id.clone())
                .or_default()
                .push(task.id.clone());
        }
    }

    index
}

/// Recursively collect all transitive dependents
fn collect_transitive_dependents(
    reverse_index: &HashMap<String, Vec<String>>,
    task_id: &str,
    visited: &mut HashSet<String>,
) {
    if let Some(dependents) = reverse_index.get(task_id) {
        for dep_id in dependents {
            if visited.insert(dep_id.clone()) {
                collect_transitive_dependents(reverse_index, dep_id, visited);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Node, Task};

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

    #[test]
    fn test_build_reverse_index_empty() {
        let graph = WorkGraph::new();
        let index = build_reverse_index(&graph);
        assert!(index.is_empty());
    }

    #[test]
    fn test_build_reverse_index_simple() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let index = build_reverse_index(&graph);
        assert_eq!(index.get("t1"), Some(&vec!["t2".to_string()]));
        assert!(index.get("t2").is_none());
    }

    #[test]
    fn test_collect_transitive_dependents() {
        let mut graph = WorkGraph::new();

        // t1 -> t2 -> t3 -> t4
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.blocked_by = vec!["t2".to_string()];
        let mut t4 = make_task("t4", "Task 4");
        t4.blocked_by = vec!["t3".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        graph.add_node(Node::Task(t4));

        let index = build_reverse_index(&graph);
        let mut visited = HashSet::new();
        collect_transitive_dependents(&index, "t1", &mut visited);

        assert_eq!(visited.len(), 3);
        assert!(visited.contains("t2"));
        assert!(visited.contains("t3"));
        assert!(visited.contains("t4"));
    }

    #[test]
    fn test_collect_transitive_dependents_with_diamond() {
        let mut graph = WorkGraph::new();

        // Diamond: t1 -> t2, t3 -> t4
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.blocked_by = vec!["t1".to_string()];
        let mut t4 = make_task("t4", "Task 4");
        t4.blocked_by = vec!["t2".to_string(), "t3".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        graph.add_node(Node::Task(t4));

        let index = build_reverse_index(&graph);
        let mut visited = HashSet::new();
        collect_transitive_dependents(&index, "t1", &mut visited);

        assert_eq!(visited.len(), 3);
        assert!(visited.contains("t2"));
        assert!(visited.contains("t3"));
        assert!(visited.contains("t4"));
    }

    #[test]
    fn test_generate_recommendation_done_task() {
        let rec = generate_recommendation(&Status::Done, 5, 10);
        assert!(rec.is_none());
    }

    #[test]
    fn test_generate_recommendation_high_priority() {
        let rec = generate_recommendation(&Status::Open, 3, 10);
        assert!(rec.is_some());
        assert!(rec.unwrap().contains("30%"));
    }

    #[test]
    fn test_generate_recommendation_medium_priority() {
        let rec = generate_recommendation(&Status::Open, 2, 20);
        assert!(rec.is_some());
        assert!(rec.unwrap().contains("10%"));
    }

    #[test]
    fn test_generate_recommendation_low_impact() {
        let rec = generate_recommendation(&Status::Open, 1, 100);
        assert!(rec.is_none());
    }

    #[test]
    fn test_generate_recommendation_in_progress_critical() {
        let rec = generate_recommendation(&Status::InProgress, 5, 20);
        assert!(rec.is_some());
        assert!(rec.unwrap().contains("Critical path"));
    }
}
