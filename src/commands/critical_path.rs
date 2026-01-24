use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::graph::{Status, WorkGraph};
use workgraph::parser::load_graph;

use super::graph_path;

/// Information about a task on the critical path
#[derive(Debug, Clone, Serialize)]
struct CriticalTask {
    id: String,
    title: String,
    status: Status,
    hours: Option<f64>,
    blocked_by: Option<String>,
}

/// Slack information for non-critical tasks
#[derive(Debug, Clone, Serialize)]
struct SlackInfo {
    id: String,
    title: String,
    slack_hours: f64,
    note: String,
}

/// JSON output structure
#[derive(Debug, Serialize)]
struct CriticalPathOutput {
    critical_path: Vec<CriticalTask>,
    task_count: usize,
    total_hours: f64,
    slack_analysis: Vec<SlackInfo>,
    cycles_skipped: Vec<Vec<String>>,
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Get non-done tasks only
    let active_tasks: Vec<_> = graph
        .tasks()
        .filter(|t| t.status != Status::Done)
        .collect();

    if active_tasks.is_empty() {
        if json {
            let output = CriticalPathOutput {
                critical_path: vec![],
                task_count: 0,
                total_hours: 0.0,
                slack_analysis: vec![],
                cycles_skipped: vec![],
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("No active tasks found.");
        }
        return Ok(());
    }

    // Build set of active task IDs for filtering
    let active_ids: HashSet<&str> = active_tasks.iter().map(|t| t.id.as_str()).collect();

    // Detect cycles among active tasks
    let cycles = detect_cycles_among_active(&graph, &active_ids);
    let cycle_nodes: HashSet<&str> = cycles.iter().flatten().map(|s| s.as_str()).collect();

    // Build dependency graph (task_id -> list of tasks it blocks)
    // This is the "forward" direction for finding paths
    let forward_index = build_forward_index(&graph, &active_ids, &cycle_nodes);

    // Find tasks with no active blockers (entry points)
    let entry_points: Vec<&str> = active_tasks
        .iter()
        .filter(|t| !cycle_nodes.contains(t.id.as_str()))
        .filter(|t| {
            t.blocked_by.iter().all(|blocker_id| {
                // Not blocked by any active non-done task
                !active_ids.contains(blocker_id.as_str())
                    || cycle_nodes.contains(blocker_id.as_str())
                    || graph
                        .get_task(blocker_id)
                        .map(|bt| bt.status == Status::Done)
                        .unwrap_or(true)
            })
        })
        .map(|t| t.id.as_str())
        .collect();

    // Calculate longest path from each entry point using dynamic programming
    // longest_path[task_id] = (total_hours, path_as_vec)
    let mut memo: HashMap<&str, (f64, Vec<String>)> = HashMap::new();

    for entry in &entry_points {
        calculate_longest_path(entry, &graph, &forward_index, &mut memo, &cycle_nodes);
    }

    // Find the overall longest path
    let (critical_path, total_hours) = if let Some((_, (hours, path))) =
        memo.iter().max_by(|a, b| a.1 .0.partial_cmp(&b.1 .0).unwrap())
    {
        (path.clone(), *hours)
    } else {
        (vec![], 0.0)
    };

    // Build critical task info
    let critical_set: HashSet<&str> = critical_path.iter().map(|s| s.as_str()).collect();
    let critical_tasks: Vec<CriticalTask> = critical_path
        .iter()
        .enumerate()
        .filter_map(|(i, task_id)| {
            graph.get_task(task_id).map(|t| {
                let blocked_by = if i == 0 {
                    None
                } else {
                    Some(critical_path[i - 1].clone())
                };
                CriticalTask {
                    id: t.id.clone(),
                    title: t.title.clone(),
                    status: t.status.clone(),
                    hours: t.estimate.as_ref().and_then(|e| e.hours),
                    blocked_by,
                }
            })
        })
        .collect();

    // Calculate slack for non-critical tasks
    let slack_analysis: Vec<SlackInfo> = active_tasks
        .iter()
        .filter(|t| !critical_set.contains(t.id.as_str()) && !cycle_nodes.contains(t.id.as_str()))
        .filter_map(|t| {
            // Slack = critical path hours - hours if this task were on the path
            // Simplified: just show the difference from total hours for this task's path
            let task_hours = t.estimate.as_ref().and_then(|e| e.hours).unwrap_or(0.0);
            let slack = total_hours - task_hours;
            if slack > 0.0 {
                Some(SlackInfo {
                    id: t.id.clone(),
                    title: t.title.clone(),
                    slack_hours: slack,
                    note: "can delay without affecting deadline".to_string(),
                })
            } else {
                None
            }
        })
        .collect();

    if json {
        let output = CriticalPathOutput {
            critical_path: critical_tasks,
            task_count: critical_path.len(),
            total_hours,
            slack_analysis,
            cycles_skipped: cycles,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if critical_path.is_empty() {
            println!("No critical path found (no active dependency chains).");
            if !cycles.is_empty() {
                println!("\nNote: {} cycle(s) were skipped.", cycles.len());
            }
            return Ok(());
        }

        println!(
            "Critical path ({} tasks, estimated {} hours):\n",
            critical_path.len(),
            format_hours(total_hours)
        );

        for (i, task_id) in critical_path.iter().enumerate() {
            if let Some(task) = graph.get_task(task_id) {
                let status_str = match task.status {
                    Status::Open | Status::InProgress => "ready",
                    Status::Blocked => "blocked",
                    Status::Done => "done",
                    Status::Failed => "failed",
                    Status::Abandoned => "abandoned",
                };

                let hours_str = task
                    .estimate
                    .as_ref()
                    .and_then(|e| e.hours)
                    .map(|h| format!(" ({}h)", h))
                    .unwrap_or_default();

                let blocked_str = if i == 0 {
                    String::new()
                } else {
                    format!(" <- blocked by {}", critical_path[i - 1])
                };

                println!(
                    "{}. [{}] {}{}{}",
                    i + 1,
                    status_str,
                    task.id,
                    hours_str,
                    blocked_str
                );
            }
        }

        if !slack_analysis.is_empty() {
            println!("\nSlack analysis:");
            for slack in &slack_analysis {
                println!(
                    "  {}: {}h slack ({})",
                    slack.id,
                    format_hours(slack.slack_hours),
                    slack.note
                );
            }
        }

        if !cycles.is_empty() {
            println!("\nNote: {} cycle(s) were skipped in analysis.", cycles.len());
        }
    }

    Ok(())
}

/// Format hours nicely (no decimals if whole number)
fn format_hours(hours: f64) -> String {
    if hours.fract() == 0.0 {
        format!("{}", hours as i64)
    } else {
        format!("{:.1}", hours)
    }
}

/// Build forward index: task_id -> tasks that it blocks (among active non-cycle tasks)
fn build_forward_index<'a>(
    graph: &'a WorkGraph,
    active_ids: &HashSet<&str>,
    cycle_nodes: &HashSet<&str>,
) -> HashMap<&'a str, Vec<&'a str>> {
    let mut index: HashMap<&str, Vec<&str>> = HashMap::new();

    for task in graph.tasks() {
        if !active_ids.contains(task.id.as_str()) || cycle_nodes.contains(task.id.as_str()) {
            continue;
        }

        // For each blocker, add this task to its forward list
        for blocker_id in &task.blocked_by {
            if active_ids.contains(blocker_id.as_str()) && !cycle_nodes.contains(blocker_id.as_str())
            {
                index
                    .entry(blocker_id.as_str())
                    .or_default()
                    .push(task.id.as_str());
            }
        }
    }

    index
}

/// Calculate longest path starting from this task using memoization
fn calculate_longest_path<'a>(
    task_id: &'a str,
    graph: &'a WorkGraph,
    forward_index: &HashMap<&'a str, Vec<&'a str>>,
    memo: &mut HashMap<&'a str, (f64, Vec<String>)>,
    cycle_nodes: &HashSet<&str>,
) -> (f64, Vec<String>) {
    // Skip cycle nodes
    if cycle_nodes.contains(task_id) {
        return (0.0, vec![]);
    }

    // Return memoized result if available
    if let Some(result) = memo.get(task_id) {
        return result.clone();
    }

    let task = match graph.get_task(task_id) {
        Some(t) => t,
        None => return (0.0, vec![]),
    };

    let task_hours = task.estimate.as_ref().and_then(|e| e.hours).unwrap_or(1.0);

    // Get tasks blocked by this one
    let blocked_tasks = forward_index.get(task_id);

    let (longest_child_hours, longest_child_path) = if let Some(children) = blocked_tasks {
        children
            .iter()
            .map(|child_id| calculate_longest_path(child_id, graph, forward_index, memo, cycle_nodes))
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
            .unwrap_or((0.0, vec![]))
    } else {
        (0.0, vec![])
    };

    let total_hours = task_hours + longest_child_hours;
    let mut path = vec![task_id.to_string()];
    path.extend(longest_child_path);

    memo.insert(task_id, (total_hours, path.clone()));
    (total_hours, path)
}

/// Detect cycles among active tasks using DFS
fn detect_cycles_among_active(graph: &WorkGraph, active_ids: &HashSet<&str>) -> Vec<Vec<String>> {
    let mut cycles = Vec::new();
    let mut visited = HashSet::new();
    let mut rec_stack = HashSet::new();
    let mut path = Vec::new();

    for task_id in active_ids {
        if !visited.contains(*task_id) {
            find_cycles_dfs(
                graph,
                task_id,
                active_ids,
                &mut visited,
                &mut rec_stack,
                &mut path,
                &mut cycles,
            );
        }
    }

    cycles
}

fn find_cycles_dfs(
    graph: &WorkGraph,
    node_id: &str,
    active_ids: &HashSet<&str>,
    visited: &mut HashSet<String>,
    rec_stack: &mut HashSet<String>,
    path: &mut Vec<String>,
    cycles: &mut Vec<Vec<String>>,
) {
    visited.insert(node_id.to_string());
    rec_stack.insert(node_id.to_string());
    path.push(node_id.to_string());

    if let Some(task) = graph.get_task(node_id) {
        for dep_id in &task.blocked_by {
            if !active_ids.contains(dep_id.as_str()) {
                continue;
            }

            if !visited.contains(dep_id) {
                find_cycles_dfs(graph, dep_id, active_ids, visited, rec_stack, path, cycles);
            } else if rec_stack.contains(dep_id) {
                // Found a cycle
                if let Some(pos) = path.iter().position(|x| x == dep_id) {
                    let cycle: Vec<String> = path[pos..].to_vec();
                    cycles.push(cycle);
                }
            }
        }
    }

    path.pop();
    rec_stack.remove(node_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Estimate, Node, Task};

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

    fn make_task_with_hours(id: &str, title: &str, hours: f64) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
            assigned: None,
            estimate: Some(Estimate {
                hours: Some(hours),
                cost: None,
            }),
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
    fn test_format_hours_whole() {
        assert_eq!(format_hours(8.0), "8");
        assert_eq!(format_hours(47.0), "47");
    }

    #[test]
    fn test_format_hours_decimal() {
        assert_eq!(format_hours(8.5), "8.5");
        assert_eq!(format_hours(47.25), "47.2");
    }

    #[test]
    fn test_empty_graph_has_no_critical_path() {
        let graph = WorkGraph::new();
        let active_ids: HashSet<&str> = HashSet::new();
        let cycles = detect_cycles_among_active(&graph, &active_ids);
        assert!(cycles.is_empty());
    }

    #[test]
    fn test_single_task_is_critical_path() {
        let mut graph = WorkGraph::new();
        let task = make_task_with_hours("t1", "Task 1", 8.0);
        graph.add_node(Node::Task(task));

        let active_ids: HashSet<&str> = vec!["t1"].into_iter().collect();
        let cycle_nodes: HashSet<&str> = HashSet::new();
        let forward_index = build_forward_index(&graph, &active_ids, &cycle_nodes);
        let mut memo = HashMap::new();

        let (hours, path) =
            calculate_longest_path("t1", &graph, &forward_index, &mut memo, &cycle_nodes);

        assert_eq!(hours, 8.0);
        assert_eq!(path, vec!["t1".to_string()]);
    }

    #[test]
    fn test_linear_chain_critical_path() {
        let mut graph = WorkGraph::new();

        // t1 (8h) -> t2 (16h) -> t3 (4h)
        let t1 = make_task_with_hours("t1", "Task 1", 8.0);
        let mut t2 = make_task_with_hours("t2", "Task 2", 16.0);
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task_with_hours("t3", "Task 3", 4.0);
        t3.blocked_by = vec!["t2".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let active_ids: HashSet<&str> = vec!["t1", "t2", "t3"].into_iter().collect();
        let cycle_nodes: HashSet<&str> = HashSet::new();
        let forward_index = build_forward_index(&graph, &active_ids, &cycle_nodes);
        let mut memo = HashMap::new();

        let (hours, path) =
            calculate_longest_path("t1", &graph, &forward_index, &mut memo, &cycle_nodes);

        assert_eq!(hours, 28.0);
        assert_eq!(
            path,
            vec!["t1".to_string(), "t2".to_string(), "t3".to_string()]
        );
    }

    #[test]
    fn test_parallel_paths_picks_longest() {
        let mut graph = WorkGraph::new();

        // t1 (8h) -> t2 (16h) -> t4 (4h)
        // t1 (8h) -> t3 (2h) -> t4 (4h)
        // Longest: t1 -> t2 -> t4 = 28h
        let t1 = make_task_with_hours("t1", "Task 1", 8.0);
        let mut t2 = make_task_with_hours("t2", "Task 2", 16.0);
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task_with_hours("t3", "Task 3", 2.0);
        t3.blocked_by = vec!["t1".to_string()];
        let mut t4 = make_task_with_hours("t4", "Task 4", 4.0);
        t4.blocked_by = vec!["t2".to_string(), "t3".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        graph.add_node(Node::Task(t4));

        let active_ids: HashSet<&str> = vec!["t1", "t2", "t3", "t4"].into_iter().collect();
        let cycle_nodes: HashSet<&str> = HashSet::new();
        let forward_index = build_forward_index(&graph, &active_ids, &cycle_nodes);
        let mut memo = HashMap::new();

        let (hours, path) =
            calculate_longest_path("t1", &graph, &forward_index, &mut memo, &cycle_nodes);

        assert_eq!(hours, 28.0);
        assert_eq!(
            path,
            vec!["t1".to_string(), "t2".to_string(), "t4".to_string()]
        );
    }

    #[test]
    fn test_done_tasks_excluded() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task_with_hours("t1", "Task 1", 8.0);
        t1.status = Status::Done;
        let mut t2 = make_task_with_hours("t2", "Task 2", 16.0);
        t2.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        // Only t2 is active
        let active_ids: HashSet<&str> = vec!["t2"].into_iter().collect();
        let cycle_nodes: HashSet<&str> = HashSet::new();
        let forward_index = build_forward_index(&graph, &active_ids, &cycle_nodes);
        let mut memo = HashMap::new();

        let (hours, path) =
            calculate_longest_path("t2", &graph, &forward_index, &mut memo, &cycle_nodes);

        assert_eq!(hours, 16.0);
        assert_eq!(path, vec!["t2".to_string()]);
    }

    #[test]
    fn test_cycle_detection() {
        let mut graph = WorkGraph::new();

        // t1 -> t2 -> t1 (cycle)
        let mut t1 = make_task("t1", "Task 1");
        t1.blocked_by = vec!["t2".to_string()];
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let active_ids: HashSet<&str> = vec!["t1", "t2"].into_iter().collect();
        let cycles = detect_cycles_among_active(&graph, &active_ids);

        assert!(!cycles.is_empty());
    }

    #[test]
    fn test_tasks_without_hours_default_to_one() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let active_ids: HashSet<&str> = vec!["t1", "t2"].into_iter().collect();
        let cycle_nodes: HashSet<&str> = HashSet::new();
        let forward_index = build_forward_index(&graph, &active_ids, &cycle_nodes);
        let mut memo = HashMap::new();

        let (hours, path) =
            calculate_longest_path("t1", &graph, &forward_index, &mut memo, &cycle_nodes);

        // Each task defaults to 1 hour
        assert_eq!(hours, 2.0);
        assert_eq!(path, vec!["t1".to_string(), "t2".to_string()]);
    }

    #[test]
    fn test_build_forward_index() {
        let mut graph = WorkGraph::new();

        // t1 -> t2, t1 -> t3
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let active_ids: HashSet<&str> = vec!["t1", "t2", "t3"].into_iter().collect();
        let cycle_nodes: HashSet<&str> = HashSet::new();
        let forward_index = build_forward_index(&graph, &active_ids, &cycle_nodes);

        let t1_blocks = forward_index.get("t1").unwrap();
        assert_eq!(t1_blocks.len(), 2);
        assert!(t1_blocks.contains(&"t2"));
        assert!(t1_blocks.contains(&"t3"));
    }
}
