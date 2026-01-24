use crate::graph::{Status, Task, WorkGraph};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::HashSet;

/// Check if a task is past its not_before timestamp (or has no timestamp)
pub fn is_time_ready(task: &Task) -> bool {
    match &task.not_before {
        None => true,
        Some(timestamp) => {
            // Parse the timestamp - if invalid, treat as ready
            match timestamp.parse::<DateTime<Utc>>() {
                Ok(not_before) => Utc::now() >= not_before,
                Err(_) => true, // Invalid timestamp = ready
            }
        }
    }
}

/// Summary of project status
#[derive(Debug, Clone, Serialize)]
pub struct ProjectSummary {
    pub open: usize,
    pub done: usize,
    pub in_progress: usize,
    pub ready: usize,
    pub blocked: usize,
    pub total_cost: f64,
    pub total_hours: f64,
}

/// Result of fitting tasks within a constraint (budget or hours)
#[derive(Debug, Clone, Serialize)]
pub struct FitResult<'a> {
    pub fits: Vec<TaskFitInfo<'a>>,
    pub exceeds: Vec<TaskFitInfo<'a>>,
    pub remaining: f64,
}

/// Information about a task and whether it fits the constraint
#[derive(Debug, Clone, Serialize)]
pub struct TaskFitInfo<'a> {
    pub id: &'a str,
    pub title: &'a str,
    pub cost: f64,
    pub hours: f64,
    pub is_ready: bool,
}

/// Get project summary (task counts and totals)
pub fn project_summary(graph: &WorkGraph) -> ProjectSummary {
    let ready = ready_tasks(graph);
    let ready_ids: HashSet<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    let mut open = 0;
    let mut done = 0;
    let mut in_progress = 0;
    let mut blocked_count = 0;
    let mut total_cost = 0.0;
    let mut total_hours = 0.0;

    for task in graph.tasks() {
        match task.status {
            Status::Open => {
                open += 1;
                if !ready_ids.contains(task.id.as_str()) {
                    blocked_count += 1;
                }
                // Add estimates for open tasks
                if let Some(ref est) = task.estimate {
                    total_cost += est.cost.unwrap_or(0.0);
                    total_hours += est.hours.unwrap_or(0.0);
                }
            }
            Status::Done => done += 1,
            Status::InProgress => in_progress += 1,
            Status::Blocked => {
                // Explicit blocked status also counts
                blocked_count += 1;
            }
            Status::Failed | Status::Abandoned => {
                // Failed and abandoned tasks are terminal states, not counted as open
            }
        }
    }

    ProjectSummary {
        open,
        done,
        in_progress,
        ready: ready.len(),
        blocked: blocked_count,
        total_cost,
        total_hours,
    }
}

/// Find tasks that fit within a budget, prioritizing ready tasks
pub fn tasks_within_budget<'a>(graph: &'a WorkGraph, budget: f64) -> FitResult<'a> {
    tasks_within_constraint(graph, budget, |t| {
        t.estimate.as_ref().and_then(|e| e.cost).unwrap_or(0.0)
    })
}

/// Find tasks that fit within available hours, prioritizing ready tasks
pub fn tasks_within_hours<'a>(graph: &'a WorkGraph, hours: f64) -> FitResult<'a> {
    tasks_within_constraint(graph, hours, |t| {
        t.estimate.as_ref().and_then(|e| e.hours).unwrap_or(0.0)
    })
}

/// Generic function to find tasks within a constraint
fn tasks_within_constraint<'a, F>(
    graph: &'a WorkGraph,
    limit: f64,
    get_value: F,
) -> FitResult<'a>
where
    F: Fn(&Task) -> f64,
{
    let ready = ready_tasks(graph);
    let ready_ids: HashSet<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    // Get all open tasks (not done, not in-progress)
    let mut open_tasks: Vec<&Task> = graph
        .tasks()
        .filter(|t| t.status == Status::Open)
        .collect();

    // Sort: ready tasks first, then by value (cost/hours) ascending
    open_tasks.sort_by(|a, b| {
        let a_ready = ready_ids.contains(a.id.as_str());
        let b_ready = ready_ids.contains(b.id.as_str());
        match (a_ready, b_ready) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => {
                let a_val = get_value(a);
                let b_val = get_value(b);
                a_val.partial_cmp(&b_val).unwrap_or(std::cmp::Ordering::Equal)
            }
        }
    });

    let mut fits = Vec::new();
    let mut exceeds = Vec::new();
    let mut remaining = limit;
    let mut completed_in_plan: HashSet<&str> = HashSet::new();

    // First pass: add ready tasks that fit
    for task in &open_tasks {
        let is_ready = ready_ids.contains(task.id.as_str());
        let value = get_value(task);
        let info = TaskFitInfo {
            id: &task.id,
            title: &task.title,
            cost: task.estimate.as_ref().and_then(|e| e.cost).unwrap_or(0.0),
            hours: task.estimate.as_ref().and_then(|e| e.hours).unwrap_or(0.0),
            is_ready,
        };

        if is_ready {
            if value <= remaining {
                remaining -= value;
                completed_in_plan.insert(&task.id);
                fits.push(info);
            } else {
                exceeds.push(info);
            }
        }
    }

    // Second pass: add blocked tasks that become unblocked by completing ready tasks
    // Keep iterating until no more tasks can be added
    let mut changed = true;
    while changed {
        changed = false;
        for task in &open_tasks {
            if completed_in_plan.contains(task.id.as_str()) {
                continue;
            }
            if ready_ids.contains(task.id.as_str()) {
                continue; // Already processed
            }

            // Check if all blockers are now completed (in our plan or actually done)
            let blockers_done = task.blocked_by.iter().all(|blocker_id| {
                completed_in_plan.contains(blocker_id.as_str())
                    || graph
                        .get_task(blocker_id)
                        .map(|t| t.status == Status::Done)
                        .unwrap_or(true)
            });

            if blockers_done {
                let value = get_value(task);
                let info = TaskFitInfo {
                    id: &task.id,
                    title: &task.title,
                    cost: task.estimate.as_ref().and_then(|e| e.cost).unwrap_or(0.0),
                    hours: task.estimate.as_ref().and_then(|e| e.hours).unwrap_or(0.0),
                    is_ready: false, // Was blocked, now unblocked by plan
                };

                if value <= remaining {
                    remaining -= value;
                    completed_in_plan.insert(&task.id);
                    fits.push(info);
                    changed = true;
                } else if !exceeds.iter().any(|e| e.id == task.id) {
                    exceeds.push(info);
                }
            }
        }
    }

    FitResult {
        fits,
        exceeds,
        remaining,
    }
}

/// Find all tasks that are ready to work on (no open blockers, past not_before)
pub fn ready_tasks(graph: &WorkGraph) -> Vec<&Task> {
    graph
        .tasks()
        .filter(|task| {
            // Must be open
            if task.status != Status::Open {
                return false;
            }
            // Must be past not_before timestamp
            if !is_time_ready(task) {
                return false;
            }
            // All blockers must be done
            task.blocked_by.iter().all(|blocker_id| {
                graph
                    .get_task(blocker_id)
                    .map(|t| t.status == Status::Done)
                    .unwrap_or(true) // If blocker doesn't exist, treat as unblocked
            })
        })
        .collect()
}

/// Find what tasks are blocking a given task
pub fn blocked_by<'a>(graph: &'a WorkGraph, task_id: &str) -> Vec<&'a Task> {
    let Some(task) = graph.get_task(task_id) else {
        return vec![];
    };

    task.blocked_by
        .iter()
        .filter_map(|id| graph.get_task(id))
        .filter(|t| t.status != Status::Done)
        .collect()
}

/// Calculate total cost of a task and all its transitive dependencies
pub fn cost_of(graph: &WorkGraph, task_id: &str) -> f64 {
    let mut visited = std::collections::HashSet::new();
    cost_of_recursive(graph, task_id, &mut visited)
}

fn cost_of_recursive(
    graph: &WorkGraph,
    task_id: &str,
    visited: &mut std::collections::HashSet<String>,
) -> f64 {
    if visited.contains(task_id) {
        return 0.0;
    }
    visited.insert(task_id.to_string());

    let Some(task) = graph.get_task(task_id) else {
        return 0.0;
    };

    let self_cost = task
        .estimate
        .as_ref()
        .and_then(|e| e.cost)
        .unwrap_or(0.0);

    let deps_cost: f64 = task
        .blocked_by
        .iter()
        .map(|dep_id| cost_of_recursive(graph, dep_id, visited))
        .sum();

    self_cost + deps_cost
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Estimate, Node};

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
    fn test_ready_tasks_empty_graph() {
        let graph = WorkGraph::new();
        let ready = ready_tasks(&graph);
        assert!(ready.is_empty());
    }

    #[test]
    fn test_ready_tasks_single_open_task() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));

        let ready = ready_tasks(&graph);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "t1");
    }

    #[test]
    fn test_ready_tasks_excludes_done() {
        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Task 1");
        task.status = Status::Done;
        graph.add_node(Node::Task(task));

        let ready = ready_tasks(&graph);
        assert!(ready.is_empty());
    }

    #[test]
    fn test_ready_tasks_excludes_blocked() {
        let mut graph = WorkGraph::new();

        let blocker = make_task("blocker", "Blocker");
        let mut blocked = make_task("blocked", "Blocked");
        blocked.blocked_by = vec!["blocker".to_string()];

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked));

        let ready = ready_tasks(&graph);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "blocker");
    }

    #[test]
    fn test_ready_tasks_unblocked_when_blocker_done() {
        let mut graph = WorkGraph::new();

        let mut blocker = make_task("blocker", "Blocker");
        blocker.status = Status::Done;

        let mut blocked = make_task("blocked", "Blocked");
        blocked.blocked_by = vec!["blocker".to_string()];

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked));

        let ready = ready_tasks(&graph);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "blocked");
    }

    #[test]
    fn test_blocked_by_returns_blockers() {
        let mut graph = WorkGraph::new();

        let blocker = make_task("blocker", "Blocker");
        let mut blocked = make_task("blocked", "Blocked");
        blocked.blocked_by = vec!["blocker".to_string()];

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked));

        let blockers = blocked_by(&graph, "blocked");
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].id, "blocker");
    }

    #[test]
    fn test_blocked_by_excludes_done_blockers() {
        let mut graph = WorkGraph::new();

        let mut blocker = make_task("blocker", "Blocker");
        blocker.status = Status::Done;

        let mut blocked = make_task("blocked", "Blocked");
        blocked.blocked_by = vec!["blocker".to_string()];

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked));

        let blockers = blocked_by(&graph, "blocked");
        assert!(blockers.is_empty());
    }

    #[test]
    fn test_cost_of_single_task() {
        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Task 1");
        task.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(1000.0),
        });
        graph.add_node(Node::Task(task));

        assert_eq!(cost_of(&graph, "t1"), 1000.0);
    }

    #[test]
    fn test_cost_of_with_dependencies() {
        let mut graph = WorkGraph::new();

        let mut dep = make_task("dep", "Dependency");
        dep.estimate = Some(Estimate {
            hours: None,
            cost: Some(500.0),
        });

        let mut task = make_task("main", "Main task");
        task.blocked_by = vec!["dep".to_string()];
        task.estimate = Some(Estimate {
            hours: None,
            cost: Some(1000.0),
        });

        graph.add_node(Node::Task(dep));
        graph.add_node(Node::Task(task));

        assert_eq!(cost_of(&graph, "main"), 1500.0);
    }

    #[test]
    fn test_cost_of_handles_cycles() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.blocked_by = vec!["t2".to_string()];
        t1.estimate = Some(Estimate {
            hours: None,
            cost: Some(100.0),
        });

        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];
        t2.estimate = Some(Estimate {
            hours: None,
            cost: Some(200.0),
        });

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        // Should not infinite loop, should count each once
        let cost = cost_of(&graph, "t1");
        assert_eq!(cost, 300.0);
    }

    #[test]
    fn test_cost_of_nonexistent_task() {
        let graph = WorkGraph::new();
        assert_eq!(cost_of(&graph, "nonexistent"), 0.0);
    }

    #[test]
    fn test_project_summary_empty() {
        let graph = WorkGraph::new();
        let summary = project_summary(&graph);
        assert_eq!(summary.open, 0);
        assert_eq!(summary.done, 0);
        assert_eq!(summary.in_progress, 0);
        assert_eq!(summary.ready, 0);
        assert_eq!(summary.blocked, 0);
        assert_eq!(summary.total_cost, 0.0);
        assert_eq!(summary.total_hours, 0.0);
    }

    #[test]
    fn test_project_summary_with_tasks() {
        let mut graph = WorkGraph::new();

        // Open task with estimate
        let mut t1 = make_task("t1", "Task 1");
        t1.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(1000.0),
        });

        // Done task (should not count in totals)
        let mut t2 = make_task("t2", "Task 2");
        t2.status = Status::Done;
        t2.estimate = Some(Estimate {
            hours: Some(5.0),
            cost: Some(500.0),
        });

        // In-progress task
        let mut t3 = make_task("t3", "Task 3");
        t3.status = Status::InProgress;
        t3.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: Some(800.0),
        });

        // Blocked task (blocked by t1)
        let mut t4 = make_task("t4", "Task 4");
        t4.blocked_by = vec!["t1".to_string()];
        t4.estimate = Some(Estimate {
            hours: Some(4.0),
            cost: Some(400.0),
        });

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        graph.add_node(Node::Task(t4));

        let summary = project_summary(&graph);
        assert_eq!(summary.open, 2); // t1, t4
        assert_eq!(summary.done, 1);
        assert_eq!(summary.in_progress, 1);
        assert_eq!(summary.ready, 1); // only t1 is ready (t4 is blocked)
        assert_eq!(summary.blocked, 1); // t4
        // Total cost of open tasks: t1 (1000) + t4 (400) = 1400
        assert_eq!(summary.total_cost, 1400.0);
        // Total hours of open tasks: t1 (10) + t4 (4) = 14
        assert_eq!(summary.total_hours, 14.0);
    }

    #[test]
    fn test_tasks_within_budget_empty() {
        let graph = WorkGraph::new();
        let result = tasks_within_budget(&graph, 1000.0);
        assert!(result.fits.is_empty());
        assert!(result.exceeds.is_empty());
        assert_eq!(result.remaining, 1000.0);
    }

    #[test]
    fn test_tasks_within_budget_basic() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.estimate = Some(Estimate {
            hours: Some(4.0),
            cost: Some(400.0),
        });

        let mut t2 = make_task("t2", "Task 2");
        t2.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: Some(800.0),
        });

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let result = tasks_within_budget(&graph, 1000.0);
        // Should fit t1 (400), leaving 600
        // t2 (800) exceeds remaining 600
        assert_eq!(result.fits.len(), 1);
        assert_eq!(result.fits[0].id, "t1");
        assert_eq!(result.exceeds.len(), 1);
        assert_eq!(result.exceeds[0].id, "t2");
        assert_eq!(result.remaining, 600.0);
    }

    #[test]
    fn test_tasks_within_budget_prioritizes_ready() {
        let mut graph = WorkGraph::new();

        // Blocker task (ready)
        let mut blocker = make_task("blocker", "Blocker");
        blocker.estimate = Some(Estimate {
            hours: Some(4.0),
            cost: Some(400.0),
        });

        // Blocked task (not ready)
        let mut blocked = make_task("blocked", "Blocked");
        blocked.blocked_by = vec!["blocker".to_string()];
        blocked.estimate = Some(Estimate {
            hours: Some(2.0),
            cost: Some(200.0),
        });

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked));

        let result = tasks_within_budget(&graph, 1000.0);
        // blocker should come first (ready), then blocked can be done
        assert_eq!(result.fits.len(), 2);
        assert_eq!(result.fits[0].id, "blocker");
        assert_eq!(result.fits[1].id, "blocked");
        assert_eq!(result.remaining, 400.0);
    }

    #[test]
    fn test_tasks_within_budget_excludes_done() {
        let mut graph = WorkGraph::new();

        let mut done = make_task("done", "Done task");
        done.status = Status::Done;
        done.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(1000.0),
        });

        let mut open = make_task("open", "Open task");
        open.estimate = Some(Estimate {
            hours: Some(5.0),
            cost: Some(500.0),
        });

        graph.add_node(Node::Task(done));
        graph.add_node(Node::Task(open));

        let result = tasks_within_budget(&graph, 1000.0);
        assert_eq!(result.fits.len(), 1);
        assert_eq!(result.fits[0].id, "open");
        assert_eq!(result.remaining, 500.0);
    }

    #[test]
    fn test_tasks_within_hours_basic() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.estimate = Some(Estimate {
            hours: Some(4.0),
            cost: Some(400.0),
        });

        let mut t2 = make_task("t2", "Task 2");
        t2.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: Some(800.0),
        });

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let result = tasks_within_hours(&graph, 10.0);
        // Should fit t1 (4h), leaving 6h
        // t2 (8h) exceeds remaining 6h
        assert_eq!(result.fits.len(), 1);
        assert_eq!(result.fits[0].id, "t1");
        assert_eq!(result.exceeds.len(), 1);
        assert_eq!(result.exceeds[0].id, "t2");
        assert_eq!(result.remaining, 6.0);
    }

    #[test]
    fn test_is_time_ready_no_timestamp() {
        let task = make_task("t1", "Task 1");
        assert!(is_time_ready(&task));
    }

    #[test]
    fn test_is_time_ready_past_timestamp() {
        let mut task = make_task("t1", "Task 1");
        // Set to a time in the past
        task.not_before = Some("2020-01-01T00:00:00Z".to_string());
        assert!(is_time_ready(&task));
    }

    #[test]
    fn test_is_time_ready_future_timestamp() {
        let mut task = make_task("t1", "Task 1");
        // Set to a time far in the future
        task.not_before = Some("2099-01-01T00:00:00Z".to_string());
        assert!(!is_time_ready(&task));
    }

    #[test]
    fn test_is_time_ready_invalid_timestamp() {
        let mut task = make_task("t1", "Task 1");
        task.not_before = Some("not-a-timestamp".to_string());
        // Invalid timestamp = treat as ready
        assert!(is_time_ready(&task));
    }

    #[test]
    fn test_ready_tasks_excludes_future_not_before() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.not_before = Some("2099-01-01T00:00:00Z".to_string());

        let t2 = make_task("t2", "Task 2");

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let ready = ready_tasks(&graph);
        // Only t2 should be ready (t1 has future not_before)
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "t2");
    }

    #[test]
    fn test_ready_tasks_includes_past_not_before() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.not_before = Some("2020-01-01T00:00:00Z".to_string());

        graph.add_node(Node::Task(t1));

        let ready = ready_tasks(&graph);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "t1");
    }
}
