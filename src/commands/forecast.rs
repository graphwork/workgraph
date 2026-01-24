use anyhow::{Context, Result};
use chrono::{Duration, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::graph::{Status, WorkGraph};
use workgraph::parser::load_graph;

use super::graph_path;
use super::velocity::calculate_velocity;

/// Default number of weeks to analyze for velocity
const DEFAULT_VELOCITY_WEEKS: usize = 4;

/// Buffer percentages for scenarios
const REALISTIC_BUFFER: f64 = 0.30;
const PESSIMISTIC_BUFFER: f64 = 0.50;

/// Remaining work breakdown
#[derive(Debug, Clone, Serialize)]
pub struct RemainingWork {
    pub open_tasks: usize,
    pub open_hours: f64,
    pub blocked_tasks: usize,
    pub blocked_hours: f64,
    pub in_progress_tasks: usize,
    pub in_progress_hours: f64,
    pub total_hours: f64,
}

/// A completion scenario
#[derive(Debug, Clone, Serialize)]
pub struct Scenario {
    pub name: String,
    pub buffer_percent: f64,
    pub estimated_hours: f64,
    pub completion_date: Option<String>,
    pub weeks_to_complete: Option<f64>,
}

/// A blocker that could delay completion
#[derive(Debug, Clone, Serialize)]
pub struct Blocker {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub hours_remaining: Option<f64>,
    pub tasks_blocked: usize,
}

/// Critical path information
#[derive(Debug, Clone, Serialize)]
pub struct CriticalPath {
    pub path: Vec<String>,
    pub total_hours: f64,
}

/// Full forecast output
#[derive(Debug, Serialize)]
pub struct ForecastOutput {
    pub remaining_work: RemainingWork,
    pub scenarios: Vec<Scenario>,
    pub blockers: Vec<Blocker>,
    pub critical_path: Option<CriticalPath>,
    pub velocity_hours_per_week: f64,
    pub has_velocity_data: bool,
    pub has_estimates: bool,
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let forecast = calculate_forecast(&graph);

    if json {
        println!("{}", serde_json::to_string_pretty(&forecast)?);
    } else {
        print_human_output(&forecast);
    }

    Ok(())
}

/// Calculate the full project forecast
pub fn calculate_forecast(graph: &WorkGraph) -> ForecastOutput {
    // Calculate remaining work breakdown
    let remaining_work = calculate_remaining_work(graph);

    // Get velocity data
    let velocity = calculate_velocity(graph, DEFAULT_VELOCITY_WEEKS);
    let has_velocity_data = velocity.average_hours_per_week > 0.0;

    // Check if we have estimates
    let has_estimates = remaining_work.total_hours > 0.0;

    // Calculate scenarios
    let scenarios = calculate_scenarios(&remaining_work, velocity.average_hours_per_week);

    // Find key blockers
    let blockers = find_key_blockers(graph);

    // Find critical path
    let critical_path = find_critical_path(graph);

    ForecastOutput {
        remaining_work,
        scenarios,
        blockers,
        critical_path,
        velocity_hours_per_week: velocity.average_hours_per_week,
        has_velocity_data,
        has_estimates,
    }
}

/// Calculate remaining work breakdown by status
fn calculate_remaining_work(graph: &WorkGraph) -> RemainingWork {
    let mut open_tasks = 0;
    let mut open_hours = 0.0;
    let mut blocked_tasks = 0;
    let mut blocked_hours = 0.0;
    let mut in_progress_tasks = 0;
    let mut in_progress_hours = 0.0;

    // Build a set of tasks that are actually blocked (have incomplete blockers)
    let blocked_ids = find_blocked_task_ids(graph);

    for task in graph.tasks() {
        if task.status == Status::Done {
            continue;
        }

        let hours = task.estimate.as_ref().and_then(|e| e.hours).unwrap_or(0.0);

        match task.status {
            Status::InProgress => {
                in_progress_tasks += 1;
                in_progress_hours += hours;
            }
            Status::Blocked => {
                blocked_tasks += 1;
                blocked_hours += hours;
            }
            Status::Open => {
                // Check if this task is actually blocked by incomplete dependencies
                if blocked_ids.contains(&task.id) {
                    blocked_tasks += 1;
                    blocked_hours += hours;
                } else {
                    open_tasks += 1;
                    open_hours += hours;
                }
            }
            Status::Done => {}
            Status::Failed | Status::Abandoned => {
                // Failed/abandoned tasks don't count toward remaining work
            }
        }
    }

    let total_hours = open_hours + blocked_hours + in_progress_hours;

    RemainingWork {
        open_tasks,
        open_hours,
        blocked_tasks,
        blocked_hours,
        in_progress_tasks,
        in_progress_hours,
        total_hours,
    }
}

/// Find task IDs that are blocked by incomplete dependencies
fn find_blocked_task_ids(graph: &WorkGraph) -> HashSet<String> {
    let mut blocked_ids = HashSet::new();

    for task in graph.tasks() {
        if task.status == Status::Done {
            continue;
        }

        // Check if any blocker is not done
        for blocker_id in &task.blocked_by {
            if let Some(blocker) = graph.get_task(blocker_id) {
                if blocker.status != Status::Done {
                    blocked_ids.insert(task.id.clone());
                    break;
                }
            }
        }
    }

    blocked_ids
}

/// Calculate completion scenarios with different buffers
fn calculate_scenarios(remaining: &RemainingWork, hours_per_week: f64) -> Vec<Scenario> {
    let mut scenarios = Vec::new();

    // Optimistic (100% - no buffer)
    scenarios.push(create_scenario(
        "Optimistic (all estimates accurate)".to_string(),
        0.0,
        remaining.total_hours,
        hours_per_week,
    ));

    // Realistic (+30% buffer)
    scenarios.push(create_scenario(
        format!("Realistic (+{}% buffer)", (REALISTIC_BUFFER * 100.0) as i32),
        REALISTIC_BUFFER,
        remaining.total_hours,
        hours_per_week,
    ));

    // Pessimistic (+50% buffer)
    scenarios.push(create_scenario(
        format!("Pessimistic (+{}% buffer)", (PESSIMISTIC_BUFFER * 100.0) as i32),
        PESSIMISTIC_BUFFER,
        remaining.total_hours,
        hours_per_week,
    ));

    scenarios
}

/// Create a single scenario
fn create_scenario(name: String, buffer: f64, base_hours: f64, hours_per_week: f64) -> Scenario {
    let estimated_hours = base_hours * (1.0 + buffer);

    let (completion_date, weeks_to_complete) = if hours_per_week > 0.0 && estimated_hours > 0.0 {
        let weeks = estimated_hours / hours_per_week;
        let days = (weeks * 7.0).ceil() as i64;
        let date = Utc::now() + Duration::days(days);
        (Some(date.format("%b %d, %Y").to_string()), Some(weeks))
    } else {
        (None, None)
    };

    Scenario {
        name,
        buffer_percent: buffer * 100.0,
        estimated_hours,
        completion_date,
        weeks_to_complete,
    }
}

/// Find key blockers that could delay completion
fn find_key_blockers(graph: &WorkGraph) -> Vec<Blocker> {
    // Build reverse index: task_id -> list of tasks that depend on it
    let reverse_index = build_reverse_index(graph);

    let mut blockers: Vec<Blocker> = Vec::new();

    for task in graph.tasks() {
        // Skip done tasks
        if task.status == Status::Done {
            continue;
        }

        // Count transitive dependents
        let mut transitive: HashSet<String> = HashSet::new();
        collect_transitive_dependents(&reverse_index, &task.id, &mut transitive);
        let tasks_blocked = transitive.len();

        // Only include tasks that block at least 2 other tasks
        if tasks_blocked >= 2 {
            let hours_remaining = task.estimate.as_ref().and_then(|e| e.hours);

            blockers.push(Blocker {
                id: task.id.clone(),
                title: task.title.clone(),
                status: task.status.clone(),
                hours_remaining,
                tasks_blocked,
            });
        }
    }

    // Sort by tasks blocked (highest first), then by hours
    blockers.sort_by(|a, b| {
        b.tasks_blocked
            .cmp(&a.tasks_blocked)
            .then_with(|| {
                let a_hours = a.hours_remaining.unwrap_or(0.0);
                let b_hours = b.hours_remaining.unwrap_or(0.0);
                b_hours.partial_cmp(&a_hours).unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    // Take top 5
    blockers.into_iter().take(5).collect()
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

/// Find the critical path (longest dependency chain by hours)
fn find_critical_path(graph: &WorkGraph) -> Option<CriticalPath> {
    // Find all entry points (tasks with no incomplete dependencies)
    let mut entry_points: Vec<&str> = Vec::new();

    for task in graph.tasks() {
        if task.status == Status::Done {
            continue;
        }

        // Check if all blockers are done (or has no blockers)
        let all_blockers_done = task.blocked_by.iter().all(|bid| {
            graph
                .get_task(bid)
                .map(|t| t.status == Status::Done)
                .unwrap_or(true)
        });

        if all_blockers_done {
            entry_points.push(&task.id);
        }
    }

    if entry_points.is_empty() {
        return None;
    }

    // Build reverse index for traversal
    let reverse_index = build_reverse_index(graph);

    // Find longest path from each entry point
    let mut longest_path: Vec<String> = Vec::new();
    let mut longest_hours: f64 = 0.0;

    for entry_id in entry_points {
        let (path, hours) = find_longest_path_from(graph, &reverse_index, entry_id);
        if hours > longest_hours || (hours == longest_hours && path.len() > longest_path.len()) {
            longest_path = path;
            longest_hours = hours;
        }
    }

    if longest_path.is_empty() {
        None
    } else {
        Some(CriticalPath {
            path: longest_path,
            total_hours: longest_hours,
        })
    }
}

/// Find the longest path starting from a given task (by total hours)
fn find_longest_path_from(
    graph: &WorkGraph,
    reverse_index: &HashMap<String, Vec<String>>,
    start_id: &str,
) -> (Vec<String>, f64) {
    let task = match graph.get_task(start_id) {
        Some(t) if t.status != Status::Done => t,
        _ => return (vec![], 0.0),
    };

    let my_hours = task.estimate.as_ref().and_then(|e| e.hours).unwrap_or(0.0);

    // Get dependents
    let dependents = reverse_index.get(start_id);

    if dependents.is_none() || dependents.unwrap().is_empty() {
        // No dependents, this is the end of the path
        return (vec![start_id.to_string()], my_hours);
    }

    // Find the longest path among all dependents
    let mut best_path: Vec<String> = Vec::new();
    let mut best_hours: f64 = 0.0;

    for dep_id in dependents.unwrap() {
        let (path, hours) = find_longest_path_from(graph, reverse_index, dep_id);
        if hours > best_hours || (hours == best_hours && path.len() > best_path.len()) {
            best_path = path;
            best_hours = hours;
        }
    }

    // Prepend current task to the best path
    let mut full_path = vec![start_id.to_string()];
    full_path.extend(best_path);

    (full_path, my_hours + best_hours)
}

fn print_human_output(forecast: &ForecastOutput) {
    println!("Project Completion Forecast:\n");

    // Remaining work
    println!("Remaining work:");
    println!(
        "  Open tasks: {} ({:.0}h estimated)",
        forecast.remaining_work.open_tasks, forecast.remaining_work.open_hours
    );
    println!(
        "  Blocked tasks: {} ({:.0}h estimated)",
        forecast.remaining_work.blocked_tasks, forecast.remaining_work.blocked_hours
    );
    println!(
        "  In progress: {} ({:.0}h estimated)",
        forecast.remaining_work.in_progress_tasks, forecast.remaining_work.in_progress_hours
    );
    println!("  Total: {:.0}h\n", forecast.remaining_work.total_hours);

    // Handle edge cases
    if forecast.remaining_work.total_hours == 0.0 {
        println!("All tasks done or no estimates available.");
        if !forecast.has_estimates {
            println!("\nNote: No hour estimates on tasks. Add estimates with --hours flag.");
        }
        return;
    }

    if !forecast.has_velocity_data {
        println!("Scenarios:\n");
        println!("  Unable to calculate completion dates - no velocity data.");
        println!("  Complete some tasks to establish velocity.\n");
    } else {
        // Scenarios
        println!("Scenarios:\n");
        for scenario in &forecast.scenarios {
            println!("  {}:", scenario.name);
            if let Some(ref date) = scenario.completion_date {
                println!("    Completion: {}", date);
            } else {
                println!("    Completion: Unable to estimate");
            }
        }
        println!();

        // Show velocity info
        println!(
            "Current velocity: {:.1}h/week\n",
            forecast.velocity_hours_per_week
        );
    }

    // Critical path
    if let Some(ref critical) = forecast.critical_path {
        if critical.path.len() > 1 {
            let path_str = if critical.path.len() <= 5 {
                critical.path.join(" -> ")
            } else {
                // Truncate long paths
                let first_three: Vec<_> = critical.path.iter().take(3).cloned().collect();
                let last = critical.path.last().unwrap();
                format!("{} -> ... -> {}", first_three.join(" -> "), last)
            };
            println!("Critical path ({:.0}h): {}\n", critical.total_hours, path_str);
        }
    }

    // Blockers
    if !forecast.blockers.is_empty() {
        println!("Blockers that could delay:");
        for blocker in &forecast.blockers {
            let hours_str = blocker
                .hours_remaining
                .map(|h| format!("{:.0}h remaining", h))
                .unwrap_or_else(|| "no estimate".to_string());
            let status_str = match blocker.status {
                Status::InProgress => "in-progress".to_string(),
                Status::Blocked => "blocked".to_string(),
                _ => String::new(),
            };
            let status_suffix = if status_str.is_empty() {
                String::new()
            } else {
                format!(", {}", status_str)
            };
            println!(
                "  - {} ({}{}, blocks {} tasks)",
                blocker.id, hours_str, status_suffix, blocker.tasks_blocked
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
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
        let mut task = make_task(id, title);
        task.estimate = Some(Estimate {
            hours: Some(hours),
            cost: None,
        });
        task
    }

    fn make_done_task(id: &str, title: &str, days_ago: i64, hours: Option<f64>) -> Task {
        let completed_at = Utc::now() - Duration::days(days_ago);
        let mut task = make_task(id, title);
        task.status = Status::Done;
        task.completed_at = Some(completed_at.to_rfc3339());
        if let Some(h) = hours {
            task.estimate = Some(Estimate {
                hours: Some(h),
                cost: None,
            });
        }
        task
    }

    #[test]
    fn test_forecast_empty_graph() {
        let graph = WorkGraph::new();
        let forecast = calculate_forecast(&graph);

        assert_eq!(forecast.remaining_work.open_tasks, 0);
        assert_eq!(forecast.remaining_work.blocked_tasks, 0);
        assert_eq!(forecast.remaining_work.in_progress_tasks, 0);
        assert_eq!(forecast.remaining_work.total_hours, 0.0);
        assert!(!forecast.has_velocity_data);
        assert!(!forecast.has_estimates);
    }

    #[test]
    fn test_forecast_all_done() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_done_task("t1", "Task 1", 1, Some(8.0))));
        graph.add_node(Node::Task(make_done_task("t2", "Task 2", 2, Some(4.0))));

        let forecast = calculate_forecast(&graph);

        assert_eq!(forecast.remaining_work.open_tasks, 0);
        assert_eq!(forecast.remaining_work.total_hours, 0.0);
    }

    #[test]
    fn test_remaining_work_calculation() {
        let mut graph = WorkGraph::new();

        // Open task
        graph.add_node(Node::Task(make_task_with_hours("t1", "Open task", 8.0)));

        // In-progress task
        let mut in_progress = make_task_with_hours("t2", "In progress", 4.0);
        in_progress.status = Status::InProgress;
        graph.add_node(Node::Task(in_progress));

        // Blocked task (explicit status)
        let mut blocked = make_task_with_hours("t3", "Blocked", 12.0);
        blocked.status = Status::Blocked;
        graph.add_node(Node::Task(blocked));

        let forecast = calculate_forecast(&graph);

        assert_eq!(forecast.remaining_work.open_tasks, 1);
        assert_eq!(forecast.remaining_work.open_hours, 8.0);
        assert_eq!(forecast.remaining_work.in_progress_tasks, 1);
        assert_eq!(forecast.remaining_work.in_progress_hours, 4.0);
        assert_eq!(forecast.remaining_work.blocked_tasks, 1);
        assert_eq!(forecast.remaining_work.blocked_hours, 12.0);
        assert_eq!(forecast.remaining_work.total_hours, 24.0);
    }

    #[test]
    fn test_blocked_by_incomplete_dependency() {
        let mut graph = WorkGraph::new();

        // Blocker task (not done)
        graph.add_node(Node::Task(make_task_with_hours("t1", "Blocker", 8.0)));

        // Dependent task (status=Open but has incomplete blocker)
        let mut dependent = make_task_with_hours("t2", "Dependent", 4.0);
        dependent.blocked_by = vec!["t1".to_string()];
        graph.add_node(Node::Task(dependent));

        let forecast = calculate_forecast(&graph);

        // t1 should be open, t2 should be counted as blocked
        assert_eq!(forecast.remaining_work.open_tasks, 1);
        assert_eq!(forecast.remaining_work.open_hours, 8.0);
        assert_eq!(forecast.remaining_work.blocked_tasks, 1);
        assert_eq!(forecast.remaining_work.blocked_hours, 4.0);
    }

    #[test]
    fn test_scenarios_with_velocity() {
        let mut graph = WorkGraph::new();

        // Some completed tasks for velocity
        graph.add_node(Node::Task(make_done_task("done1", "Done 1", 1, Some(10.0))));
        graph.add_node(Node::Task(make_done_task("done2", "Done 2", 3, Some(10.0))));
        graph.add_node(Node::Task(make_done_task("done3", "Done 3", 5, Some(10.0))));
        graph.add_node(Node::Task(make_done_task("done4", "Done 4", 8, Some(10.0))));

        // Open task
        graph.add_node(Node::Task(make_task_with_hours("open1", "Open", 100.0)));

        let forecast = calculate_forecast(&graph);

        assert!(forecast.has_velocity_data);
        assert_eq!(forecast.scenarios.len(), 3);

        // Check scenario names
        assert!(forecast.scenarios[0].name.contains("Optimistic"));
        assert!(forecast.scenarios[1].name.contains("Realistic"));
        assert!(forecast.scenarios[2].name.contains("Pessimistic"));

        // Check estimated hours include buffers
        assert_eq!(forecast.scenarios[0].estimated_hours, 100.0);
        assert_eq!(forecast.scenarios[1].estimated_hours, 130.0); // +30%
        assert_eq!(forecast.scenarios[2].estimated_hours, 150.0); // +50%

        // Should have completion dates since we have velocity
        assert!(forecast.scenarios[0].completion_date.is_some());
    }

    #[test]
    fn test_scenarios_without_velocity() {
        let mut graph = WorkGraph::new();

        // Only open tasks, no completed ones
        graph.add_node(Node::Task(make_task_with_hours("t1", "Task 1", 100.0)));

        let forecast = calculate_forecast(&graph);

        assert!(!forecast.has_velocity_data);
        // Scenarios should exist but without completion dates
        assert_eq!(forecast.scenarios.len(), 3);
        assert!(forecast.scenarios[0].completion_date.is_none());
    }

    #[test]
    fn test_find_key_blockers() {
        let mut graph = WorkGraph::new();

        // Root blocker that blocks many tasks
        graph.add_node(Node::Task(make_task_with_hours("root", "Root blocker", 8.0)));

        // Tasks that depend on root
        for i in 1..=5 {
            let mut task = make_task_with_hours(&format!("t{}", i), &format!("Task {}", i), 4.0);
            task.blocked_by = vec!["root".to_string()];
            graph.add_node(Node::Task(task));
        }

        let forecast = calculate_forecast(&graph);

        // Root should be identified as a blocker
        assert!(!forecast.blockers.is_empty());
        assert_eq!(forecast.blockers[0].id, "root");
        assert_eq!(forecast.blockers[0].tasks_blocked, 5);
    }

    #[test]
    fn test_critical_path_linear() {
        let mut graph = WorkGraph::new();

        // Linear chain: t1 -> t2 -> t3
        graph.add_node(Node::Task(make_task_with_hours("t1", "Task 1", 8.0)));

        let mut t2 = make_task_with_hours("t2", "Task 2", 4.0);
        t2.blocked_by = vec!["t1".to_string()];
        graph.add_node(Node::Task(t2));

        let mut t3 = make_task_with_hours("t3", "Task 3", 2.0);
        t3.blocked_by = vec!["t2".to_string()];
        graph.add_node(Node::Task(t3));

        let forecast = calculate_forecast(&graph);

        assert!(forecast.critical_path.is_some());
        let critical = forecast.critical_path.unwrap();
        assert_eq!(critical.path, vec!["t1", "t2", "t3"]);
        assert_eq!(critical.total_hours, 14.0);
    }

    #[test]
    fn test_critical_path_branching() {
        let mut graph = WorkGraph::new();

        // Branching: t1 -> t2 (2h) and t1 -> t3 (10h)
        // Critical path should be t1 -> t3
        graph.add_node(Node::Task(make_task_with_hours("t1", "Task 1", 4.0)));

        let mut t2 = make_task_with_hours("t2", "Task 2", 2.0);
        t2.blocked_by = vec!["t1".to_string()];
        graph.add_node(Node::Task(t2));

        let mut t3 = make_task_with_hours("t3", "Task 3", 10.0);
        t3.blocked_by = vec!["t1".to_string()];
        graph.add_node(Node::Task(t3));

        let forecast = calculate_forecast(&graph);

        assert!(forecast.critical_path.is_some());
        let critical = forecast.critical_path.unwrap();
        assert_eq!(critical.path, vec!["t1", "t3"]);
        assert_eq!(critical.total_hours, 14.0);
    }

    #[test]
    fn test_no_estimates() {
        let mut graph = WorkGraph::new();

        // Task without hours estimate
        graph.add_node(Node::Task(make_task("t1", "Task 1")));
        graph.add_node(Node::Task(make_task("t2", "Task 2")));

        let forecast = calculate_forecast(&graph);

        assert!(!forecast.has_estimates);
        assert_eq!(forecast.remaining_work.total_hours, 0.0);
        assert_eq!(forecast.remaining_work.open_tasks, 2);
    }

    #[test]
    fn test_json_serialization() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task_with_hours("t1", "Task 1", 8.0)));

        let forecast = calculate_forecast(&graph);
        let json = serde_json::to_string(&forecast).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert!(parsed["remaining_work"]["open_tasks"].is_number());
        assert!(parsed["scenarios"].is_array());
        assert!(parsed["blockers"].is_array());
    }

    #[test]
    fn test_blocker_minimum_threshold() {
        let mut graph = WorkGraph::new();

        // Blocker that only blocks 1 task (should not appear in blockers list)
        graph.add_node(Node::Task(make_task_with_hours("root", "Root", 8.0)));

        let mut t1 = make_task_with_hours("t1", "Task 1", 4.0);
        t1.blocked_by = vec!["root".to_string()];
        graph.add_node(Node::Task(t1));

        let forecast = calculate_forecast(&graph);

        // Root blocks only 1 task, threshold is 2
        assert!(forecast.blockers.is_empty());
    }
}
