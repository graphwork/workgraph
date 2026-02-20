use crate::graph::{Status, Task, WorkGraph};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Check if a task is past its not_before and ready_after timestamps (or has no timestamps)
pub fn is_time_ready(task: &Task) -> bool {
    let now = Utc::now();

    // Check not_before
    if let Some(timestamp) = &task.not_before
        && let Ok(not_before) = timestamp.parse::<DateTime<Utc>>()
        && now < not_before
    {
        return false;
    }
    // Invalid timestamp = treat as ready (don't block)

    // Check ready_after (set by loop edges with delays)
    if let Some(timestamp) = &task.ready_after
        && let Ok(ready_after) = timestamp.parse::<DateTime<Utc>>()
        && now < ready_after
    {
        return false;
    }
    // Invalid timestamp = treat as ready (don't block)

    true
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
fn tasks_within_constraint<'a, F>(graph: &'a WorkGraph, limit: f64, get_value: F) -> FitResult<'a>
where
    F: Fn(&Task) -> f64,
{
    let ready = ready_tasks(graph);
    let ready_ids: HashSet<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    // Get all open tasks (not done, not in-progress)
    let mut open_tasks: Vec<&Task> = graph.tasks().filter(|t| t.status == Status::Open).collect();

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
                a_val
                    .partial_cmp(&b_val)
                    .unwrap_or(std::cmp::Ordering::Equal)
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

            // Check if all blockers are now resolved (in our plan or terminal)
            let blockers_done = task.blocked_by.iter().all(|blocker_id| {
                completed_in_plan.contains(blocker_id.as_str())
                    || graph
                        .get_task(blocker_id)
                        .map(|t| t.status.is_terminal())
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

/// Build a reverse dependency index: maps each task ID to the list of tasks that depend on it.
pub fn build_reverse_index(graph: &WorkGraph) -> HashMap<String, Vec<String>> {
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

/// Find all tasks that are ready to work on (no open blockers, past not_before)
pub fn ready_tasks(graph: &WorkGraph) -> Vec<&Task> {
    graph
        .tasks()
        .filter(|task| {
            // Must be open
            if task.status != Status::Open {
                return false;
            }
            // Must not be paused
            if task.paused {
                return false;
            }
            // Must be past not_before timestamp
            if !is_time_ready(task) {
                return false;
            }
            // All blockers must be terminal (done, failed, or abandoned)
            task.blocked_by.iter().all(|blocker_id| {
                graph
                    .get_task(blocker_id)
                    .map(|t| t.status.is_terminal())
                    .unwrap_or(true) // If blocker doesn't exist, treat as unblocked
            })
        })
        .collect()
}

/// Check whether a single blocked_by dependency is satisfied (terminal).
///
/// Handles both local and remote (`peer:task-id`) references.
/// For remote refs, resolves via federation config using IPC or direct file access.
pub fn is_blocker_satisfied(
    blocker_id: &str,
    graph: &WorkGraph,
    workgraph_dir: Option<&Path>,
) -> bool {
    if let Some((peer_name, remote_task_id)) = crate::federation::parse_remote_ref(blocker_id) {
        // Cross-repo dependency
        let Some(wg_dir) = workgraph_dir else {
            return false; // Can't resolve without workgraph dir; treat as blocked
        };
        let remote = crate::federation::resolve_remote_task_status(peer_name, remote_task_id, wg_dir);
        remote.status.is_terminal()
    } else {
        // Local dependency
        graph
            .get_task(blocker_id)
            .map(|t| t.status.is_terminal())
            .unwrap_or(true) // If blocker doesn't exist, treat as unblocked
    }
}

/// Find all tasks that are ready to work on, resolving cross-repo dependencies.
///
/// This is the cross-repo-aware variant of `ready_tasks()`. The coordinator
/// should use this version so that tasks blocked by remote `peer:task-id`
/// references are correctly resolved.
pub fn ready_tasks_with_peers<'a>(graph: &'a WorkGraph, workgraph_dir: &Path) -> Vec<&'a Task> {
    graph
        .tasks()
        .filter(|task| {
            if task.status != Status::Open {
                return false;
            }
            if task.paused {
                return false;
            }
            if !is_time_ready(task) {
                return false;
            }
            task.blocked_by
                .iter()
                .all(|blocker_id| is_blocker_satisfied(blocker_id, graph, Some(workgraph_dir)))
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
        .filter(|t| !t.status.is_terminal())
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

    let self_cost = task.estimate.as_ref().and_then(|e| e.cost).unwrap_or(0.0);

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
            ..Task::default()
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
    fn test_blocked_by_excludes_failed_blockers() {
        let mut graph = WorkGraph::new();

        let mut blocker = make_task("blocker", "Blocker");
        blocker.status = Status::Failed;

        let mut blocked = make_task("blocked", "Blocked");
        blocked.blocked_by = vec!["blocker".to_string()];

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked));

        let blockers = blocked_by(&graph, "blocked");
        assert!(
            blockers.is_empty(),
            "Failed blockers should not block dependents"
        );
    }

    #[test]
    fn test_blocked_by_excludes_abandoned_blockers() {
        let mut graph = WorkGraph::new();

        let mut blocker = make_task("blocker", "Blocker");
        blocker.status = Status::Abandoned;

        let mut blocked = make_task("blocked", "Blocked");
        blocked.blocked_by = vec!["blocker".to_string()];

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked));

        let blockers = blocked_by(&graph, "blocked");
        assert!(
            blockers.is_empty(),
            "Abandoned blockers should not block dependents"
        );
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

    // ========== Transitive blocker tests ==========

    #[test]
    fn test_ready_tasks_transitive_blockers_3_levels() {
        // A blocked by B, B blocked by C — only C should be ready
        let mut graph = WorkGraph::new();

        let c = make_task("c", "Level 0 (root)");
        let mut b = make_task("b", "Level 1");
        b.blocked_by = vec!["c".to_string()];
        let mut a = make_task("a", "Level 2");
        a.blocked_by = vec!["b".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let ready = ready_tasks(&graph);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "c");
    }

    #[test]
    fn test_ready_tasks_transitive_blockers_4_levels() {
        // d -> c -> b -> a: only d should be ready
        let mut graph = WorkGraph::new();

        let d = make_task("d", "Level 0");
        let mut c = make_task("c", "Level 1");
        c.blocked_by = vec!["d".to_string()];
        let mut b = make_task("b", "Level 2");
        b.blocked_by = vec!["c".to_string()];
        let mut a = make_task("a", "Level 3");
        a.blocked_by = vec!["b".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let ready = ready_tasks(&graph);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "d");
    }

    #[test]
    fn test_ready_tasks_transitive_partial_done() {
        // d(Done) -> c -> b -> a: c should be ready now
        let mut graph = WorkGraph::new();

        let mut d = make_task("d", "Level 0");
        d.status = Status::Done;
        let mut c = make_task("c", "Level 1");
        c.blocked_by = vec!["d".to_string()];
        let mut b = make_task("b", "Level 2");
        b.blocked_by = vec!["c".to_string()];
        let mut a = make_task("a", "Level 3");
        a.blocked_by = vec!["b".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let ready = ready_tasks(&graph);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "c");
    }

    // ========== Multiple blockers with mixed states ==========

    #[test]
    fn test_ready_tasks_multiple_blockers_some_done() {
        // Task blocked by b1(Done) and b2(Open) — should NOT be ready
        let mut graph = WorkGraph::new();

        let mut b1 = make_task("b1", "Blocker 1");
        b1.status = Status::Done;
        let b2 = make_task("b2", "Blocker 2");
        let mut task = make_task("t", "Blocked task");
        task.blocked_by = vec!["b1".to_string(), "b2".to_string()];

        graph.add_node(Node::Task(b1));
        graph.add_node(Node::Task(b2));
        graph.add_node(Node::Task(task));

        let ready = ready_tasks(&graph);
        let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
        assert!(ready_ids.contains(&"b2"), "b2 should be ready");
        assert!(
            !ready_ids.contains(&"t"),
            "t should NOT be ready (b2 still open)"
        );
    }

    #[test]
    fn test_ready_tasks_multiple_blockers_all_done() {
        // Task blocked by b1(Done) and b2(Done) — SHOULD be ready
        let mut graph = WorkGraph::new();

        let mut b1 = make_task("b1", "Blocker 1");
        b1.status = Status::Done;
        let mut b2 = make_task("b2", "Blocker 2");
        b2.status = Status::Done;
        let mut task = make_task("t", "Blocked task");
        task.blocked_by = vec!["b1".to_string(), "b2".to_string()];

        graph.add_node(Node::Task(b1));
        graph.add_node(Node::Task(b2));
        graph.add_node(Node::Task(task));

        let ready = ready_tasks(&graph);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "t");
    }

    #[test]
    fn test_ready_tasks_multiple_blockers_mixed_statuses() {
        // Task blocked by done, in-progress, and failed tasks
        let mut graph = WorkGraph::new();

        let mut b_done = make_task("b-done", "Done blocker");
        b_done.status = Status::Done;
        let mut b_ip = make_task("b-ip", "InProgress blocker");
        b_ip.status = Status::InProgress;
        let mut b_failed = make_task("b-failed", "Failed blocker");
        b_failed.status = Status::Failed;

        let mut task = make_task("t", "Blocked task");
        task.blocked_by = vec![
            "b-done".to_string(),
            "b-ip".to_string(),
            "b-failed".to_string(),
        ];

        graph.add_node(Node::Task(b_done));
        graph.add_node(Node::Task(b_ip));
        graph.add_node(Node::Task(b_failed));
        graph.add_node(Node::Task(task));

        // Terminal states (Done, Failed, Abandoned) count as resolved.
        // InProgress still blocks, so t should NOT be ready.
        let ready = ready_tasks(&graph);
        let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
        assert!(
            !ready_ids.contains(&"t"),
            "t should NOT be ready (b-ip is still in-progress)"
        );
    }

    #[test]
    fn test_ready_tasks_failed_blocker_unblocks() {
        // A task whose only blocker has failed should become ready
        let mut graph = WorkGraph::new();

        let mut b_failed = make_task("b-failed", "Failed blocker");
        b_failed.status = Status::Failed;

        let mut task = make_task("t", "Blocked task");
        task.blocked_by = vec!["b-failed".to_string()];

        graph.add_node(Node::Task(b_failed));
        graph.add_node(Node::Task(task));

        let ready = ready_tasks(&graph);
        let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
        assert!(
            ready_ids.contains(&"t"),
            "t should be ready when blocker has failed"
        );
    }

    #[test]
    fn test_ready_tasks_abandoned_blocker_unblocks() {
        // A task whose only blocker was abandoned should become ready
        let mut graph = WorkGraph::new();

        let mut b_abandoned = make_task("b-abandoned", "Abandoned blocker");
        b_abandoned.status = Status::Abandoned;

        let mut task = make_task("t", "Blocked task");
        task.blocked_by = vec!["b-abandoned".to_string()];

        graph.add_node(Node::Task(b_abandoned));
        graph.add_node(Node::Task(task));

        let ready = ready_tasks(&graph);
        let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
        assert!(
            ready_ids.contains(&"t"),
            "t should be ready when blocker was abandoned"
        );
    }

    // ========== Orphan blocker tests ==========

    #[test]
    fn test_ready_tasks_orphan_blocker_nonexistent() {
        // Task references a blocker that doesn't exist in the graph
        // Current behavior: nonexistent blocker treated as done (unwrap_or(true))
        let mut graph = WorkGraph::new();

        let mut task = make_task("t", "Task with ghost blocker");
        task.blocked_by = vec!["nonexistent".to_string()];

        graph.add_node(Node::Task(task));

        let ready = ready_tasks(&graph);
        assert_eq!(
            ready.len(),
            1,
            "Task with nonexistent blocker should be ready"
        );
        assert_eq!(ready[0].id, "t");
    }

    #[test]
    fn test_ready_tasks_mix_real_and_orphan_blockers() {
        // One real open blocker + one nonexistent blocker
        let mut graph = WorkGraph::new();

        let real_blocker = make_task("real", "Real blocker");
        let mut task = make_task("t", "Mixed blockers");
        task.blocked_by = vec!["real".to_string(), "ghost".to_string()];

        graph.add_node(Node::Task(real_blocker));
        graph.add_node(Node::Task(task));

        let ready = ready_tasks(&graph);
        let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
        // "real" is Open so "t" is still blocked
        assert!(ready_ids.contains(&"real"));
        assert!(!ready_ids.contains(&"t"));
    }

    #[test]
    fn test_blocked_by_with_orphan_blocker() {
        // blocked_by() should silently skip nonexistent blockers
        let mut graph = WorkGraph::new();

        let mut task = make_task("t", "Task");
        task.blocked_by = vec!["ghost1".to_string(), "ghost2".to_string()];

        graph.add_node(Node::Task(task));

        let blockers = blocked_by(&graph, "t");
        assert!(
            blockers.is_empty(),
            "Nonexistent blockers should be filtered out"
        );
    }

    #[test]
    fn test_blocked_by_nonexistent_task() {
        let graph = WorkGraph::new();
        let blockers = blocked_by(&graph, "no-such-task");
        assert!(blockers.is_empty());
    }

    // ========== build_reverse_index() direct tests ==========

    #[test]
    fn test_build_reverse_index_empty_graph() {
        let graph = WorkGraph::new();
        let index = build_reverse_index(&graph);
        assert!(index.is_empty());
    }

    #[test]
    fn test_build_reverse_index_no_dependencies() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "A")));
        graph.add_node(Node::Task(make_task("b", "B")));

        let index = build_reverse_index(&graph);
        assert!(
            index.is_empty(),
            "No dependencies means empty reverse index"
        );
    }

    #[test]
    fn test_build_reverse_index_linear_chain() {
        // a -> b -> c (c blocked_by b, b blocked_by a)
        let mut graph = WorkGraph::new();

        let a = make_task("a", "A");
        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];
        let mut c = make_task("c", "C");
        c.blocked_by = vec!["b".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let index = build_reverse_index(&graph);
        // "a" is depended on by "b"
        assert_eq!(index.get("a").unwrap(), &vec!["b".to_string()]);
        // "b" is depended on by "c"
        assert_eq!(index.get("b").unwrap(), &vec!["c".to_string()]);
        // "c" is not depended on by anything
        assert!(!index.contains_key("c"));
    }

    #[test]
    fn test_build_reverse_index_branching() {
        // "root" has two dependents: "left" and "right"
        let mut graph = WorkGraph::new();

        let root = make_task("root", "Root");
        let mut left = make_task("left", "Left");
        left.blocked_by = vec!["root".to_string()];
        let mut right = make_task("right", "Right");
        right.blocked_by = vec!["root".to_string()];

        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(left));
        graph.add_node(Node::Task(right));

        let index = build_reverse_index(&graph);
        let dependents = index.get("root").unwrap();
        assert_eq!(dependents.len(), 2);
        assert!(dependents.contains(&"left".to_string()));
        assert!(dependents.contains(&"right".to_string()));
    }

    #[test]
    fn test_build_reverse_index_diamond() {
        // Diamond: a -> b, a -> c, b -> d, c -> d
        let mut graph = WorkGraph::new();

        let a = make_task("a", "A");
        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];
        let mut c = make_task("c", "C");
        c.blocked_by = vec!["a".to_string()];
        let mut d = make_task("d", "D");
        d.blocked_by = vec!["b".to_string(), "c".to_string()];

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let index = build_reverse_index(&graph);
        let a_deps = index.get("a").unwrap();
        assert_eq!(a_deps.len(), 2);
        assert!(a_deps.contains(&"b".to_string()));
        assert!(a_deps.contains(&"c".to_string()));

        assert_eq!(index.get("b").unwrap(), &vec!["d".to_string()]);
        assert_eq!(index.get("c").unwrap(), &vec!["d".to_string()]);
        assert!(!index.contains_key("d"));
    }

    // ========== tasks_within_constraint() edge cases ==========

    #[test]
    fn test_tasks_within_budget_zero_budget() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.estimate = Some(Estimate {
            hours: Some(1.0),
            cost: Some(100.0),
        });

        graph.add_node(Node::Task(t1));

        let result = tasks_within_budget(&graph, 0.0);
        // Zero-cost tasks would fit (100 > 0), so t1 should exceed
        assert!(result.fits.is_empty());
        assert_eq!(result.exceeds.len(), 1);
        assert_eq!(result.remaining, 0.0);
    }

    #[test]
    fn test_tasks_within_budget_zero_cost_task_zero_budget() {
        // A task with no estimate (defaults to 0 cost) should fit in zero budget
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "No estimate task");
        graph.add_node(Node::Task(t1));

        let result = tasks_within_budget(&graph, 0.0);
        assert_eq!(
            result.fits.len(),
            1,
            "Zero-cost task should fit in zero budget"
        );
        assert_eq!(result.fits[0].id, "t1");
        assert_eq!(result.remaining, 0.0);
    }

    #[test]
    fn test_tasks_within_budget_negative_budget() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task");
        graph.add_node(Node::Task(t1));

        let result = tasks_within_budget(&graph, -10.0);
        // Even zero-cost task: 0.0 <= -10.0 is false, so nothing fits
        assert!(result.fits.is_empty());
        assert_eq!(result.remaining, -10.0);
    }

    #[test]
    fn test_tasks_within_budget_none_estimates() {
        // Tasks with None estimates should default to 0 cost and always fit
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "No estimate");
        let mut t2 = make_task("t2", "Partial estimate");
        t2.estimate = Some(Estimate {
            hours: Some(5.0),
            cost: None, // cost is None
        });

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let result = tasks_within_budget(&graph, 50.0);
        // Both should fit: t1 costs 0, t2 costs 0 (None -> 0.0)
        assert_eq!(result.fits.len(), 2);
        assert_eq!(result.remaining, 50.0);
    }

    #[test]
    fn test_tasks_within_hours_none_estimates() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Only cost");
        t1.estimate = Some(Estimate {
            hours: None,
            cost: Some(999.0),
        });

        graph.add_node(Node::Task(t1));

        let result = tasks_within_hours(&graph, 10.0);
        // hours is None -> 0.0, so it fits
        assert_eq!(result.fits.len(), 1);
        assert_eq!(result.remaining, 10.0);
    }

    #[test]
    fn test_tasks_within_budget_exact_fit() {
        // Task cost exactly equals remaining budget
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Exact fit");
        t1.estimate = Some(Estimate {
            hours: None,
            cost: Some(500.0),
        });

        graph.add_node(Node::Task(t1));

        let result = tasks_within_budget(&graph, 500.0);
        assert_eq!(result.fits.len(), 1);
        assert_eq!(result.remaining, 0.0);
    }

    #[test]
    fn test_tasks_within_budget_tiny_overshoot() {
        // Budget is just barely less than cost (floating point boundary)
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Tiny overshoot");
        t1.estimate = Some(Estimate {
            hours: None,
            cost: Some(100.0),
        });

        graph.add_node(Node::Task(t1));

        // Budget is 99.99999999 — just under 100
        let result = tasks_within_budget(&graph, 99.99999999);
        assert_eq!(
            result.exceeds.len(),
            1,
            "100.0 > 99.99999999, should not fit"
        );
        assert!(result.fits.is_empty());
    }

    #[test]
    fn test_tasks_within_budget_cascading_unblock() {
        // a (ready) -> b (blocked by a) -> c (blocked by b)
        // All should fit if budget allows, since completing a unblocks b, which unblocks c
        let mut graph = WorkGraph::new();

        let mut a = make_task("a", "A");
        a.estimate = Some(Estimate {
            hours: None,
            cost: Some(10.0),
        });
        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];
        b.estimate = Some(Estimate {
            hours: None,
            cost: Some(20.0),
        });
        let mut c = make_task("c", "C");
        c.blocked_by = vec!["b".to_string()];
        c.estimate = Some(Estimate {
            hours: None,
            cost: Some(30.0),
        });

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let result = tasks_within_budget(&graph, 100.0);
        assert_eq!(
            result.fits.len(),
            3,
            "All three should fit within cascading plan"
        );
        assert_eq!(result.fits[0].id, "a");
        assert_eq!(result.fits[1].id, "b");
        assert_eq!(result.fits[2].id, "c");
        assert_eq!(result.remaining, 40.0);
    }

    // ========== cost_of() edge cases ==========

    #[test]
    fn test_cost_of_nonexistent_dep_in_chain() {
        // Task references a nonexistent dependency
        let mut graph = WorkGraph::new();

        let mut task = make_task("t", "Task");
        task.blocked_by = vec!["ghost".to_string()];
        task.estimate = Some(Estimate {
            hours: None,
            cost: Some(100.0),
        });

        graph.add_node(Node::Task(task));

        // "ghost" doesn't exist -> cost_of returns 0 for it
        assert_eq!(cost_of(&graph, "t"), 100.0);
    }

    #[test]
    fn test_cost_of_self_blocking() {
        // Task references itself as a blocker (degenerate cycle of length 1)
        let mut graph = WorkGraph::new();

        let mut task = make_task("self", "Self-blocking");
        task.blocked_by = vec!["self".to_string()];
        task.estimate = Some(Estimate {
            hours: None,
            cost: Some(50.0),
        });

        graph.add_node(Node::Task(task));

        // Should not infinite loop; visited set catches it
        let cost = cost_of(&graph, "self");
        assert_eq!(cost, 50.0);
    }

    #[test]
    fn test_cost_of_deep_chain() {
        // Chain of 5: e -> d -> c -> b -> a, each costs 10
        let mut graph = WorkGraph::new();

        let mut a = make_task("a", "A");
        a.estimate = Some(Estimate {
            hours: None,
            cost: Some(10.0),
        });

        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];
        b.estimate = Some(Estimate {
            hours: None,
            cost: Some(10.0),
        });

        let mut c = make_task("c", "C");
        c.blocked_by = vec!["b".to_string()];
        c.estimate = Some(Estimate {
            hours: None,
            cost: Some(10.0),
        });

        let mut d = make_task("d", "D");
        d.blocked_by = vec!["c".to_string()];
        d.estimate = Some(Estimate {
            hours: None,
            cost: Some(10.0),
        });

        let mut e = make_task("e", "E");
        e.blocked_by = vec!["d".to_string()];
        e.estimate = Some(Estimate {
            hours: None,
            cost: Some(10.0),
        });

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));
        graph.add_node(Node::Task(e));

        assert_eq!(cost_of(&graph, "e"), 50.0);
    }

    #[test]
    fn test_cost_of_diamond_no_double_count() {
        // Diamond: a -> b, a -> c, b -> d, c -> d
        // d should count a,b,c,d each once
        let mut graph = WorkGraph::new();

        let mut a = make_task("a", "A");
        a.estimate = Some(Estimate {
            hours: None,
            cost: Some(10.0),
        });

        let mut b = make_task("b", "B");
        b.blocked_by = vec!["a".to_string()];
        b.estimate = Some(Estimate {
            hours: None,
            cost: Some(20.0),
        });

        let mut c = make_task("c", "C");
        c.blocked_by = vec!["a".to_string()];
        c.estimate = Some(Estimate {
            hours: None,
            cost: Some(30.0),
        });

        let mut d = make_task("d", "D");
        d.blocked_by = vec!["b".to_string(), "c".to_string()];
        d.estimate = Some(Estimate {
            hours: None,
            cost: Some(40.0),
        });

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        // d(40) + b(20) + c(30) + a(10) = 100 (a counted only once)
        assert_eq!(cost_of(&graph, "d"), 100.0);
    }

    #[test]
    fn test_cost_of_no_estimate() {
        // Task with no estimate should contribute 0
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t", "No estimate")));
        assert_eq!(cost_of(&graph, "t"), 0.0);
    }

    // ========== ready_tasks with various terminal statuses ==========

    #[test]
    fn test_ready_tasks_excludes_in_progress() {
        let mut graph = WorkGraph::new();
        let mut task = make_task("t", "In progress");
        task.status = Status::InProgress;
        graph.add_node(Node::Task(task));

        let ready = ready_tasks(&graph);
        assert!(ready.is_empty(), "InProgress tasks should not be ready");
    }

    #[test]
    fn test_ready_tasks_excludes_failed() {
        let mut graph = WorkGraph::new();
        let mut task = make_task("t", "Failed");
        task.status = Status::Failed;
        graph.add_node(Node::Task(task));

        let ready = ready_tasks(&graph);
        assert!(ready.is_empty(), "Failed tasks should not be ready");
    }

    #[test]
    fn test_ready_tasks_excludes_abandoned() {
        let mut graph = WorkGraph::new();
        let mut task = make_task("t", "Abandoned");
        task.status = Status::Abandoned;
        graph.add_node(Node::Task(task));

        let ready = ready_tasks(&graph);
        assert!(ready.is_empty(), "Abandoned tasks should not be ready");
    }

    #[test]
    fn test_ready_tasks_excludes_done_status() {
        let mut graph = WorkGraph::new();
        let mut task = make_task("t", "Done task");
        task.status = Status::Done;
        graph.add_node(Node::Task(task));

        let ready = ready_tasks(&graph);
        assert!(ready.is_empty(), "Done tasks should not be ready");
    }

    // ========== is_time_ready with ready_after ==========

    #[test]
    fn test_is_time_ready_future_ready_after() {
        let mut task = make_task("t", "Task");
        task.ready_after = Some("2099-01-01T00:00:00Z".to_string());
        assert!(!is_time_ready(&task), "Future ready_after should block");
    }

    #[test]
    fn test_is_time_ready_past_ready_after() {
        let mut task = make_task("t", "Task");
        task.ready_after = Some("2020-01-01T00:00:00Z".to_string());
        assert!(is_time_ready(&task), "Past ready_after should be ready");
    }

    #[test]
    fn test_is_time_ready_invalid_ready_after() {
        let mut task = make_task("t", "Task");
        task.ready_after = Some("garbage".to_string());
        assert!(
            is_time_ready(&task),
            "Invalid ready_after should be treated as ready"
        );
    }

    #[test]
    fn test_is_time_ready_both_timestamps_past() {
        let mut task = make_task("t", "Task");
        task.not_before = Some("2020-01-01T00:00:00Z".to_string());
        task.ready_after = Some("2020-06-01T00:00:00Z".to_string());
        assert!(is_time_ready(&task));
    }

    #[test]
    fn test_is_time_ready_not_before_past_ready_after_future() {
        let mut task = make_task("t", "Task");
        task.not_before = Some("2020-01-01T00:00:00Z".to_string());
        task.ready_after = Some("2099-01-01T00:00:00Z".to_string());
        assert!(
            !is_time_ready(&task),
            "Future ready_after should still block"
        );
    }

    // -----------------------------------------------------------------------
    // is_blocker_satisfied tests
    // -----------------------------------------------------------------------

    #[test]
    fn is_blocker_satisfied_local_done() {
        let mut graph = WorkGraph::new();
        let mut t = make_task("blocker", "Blocker");
        t.status = Status::Done;
        graph.add_node(Node::Task(t));

        assert!(is_blocker_satisfied("blocker", &graph, None));
    }

    #[test]
    fn is_blocker_satisfied_local_open() {
        let mut graph = WorkGraph::new();
        let t = make_task("blocker", "Blocker");
        graph.add_node(Node::Task(t));

        assert!(!is_blocker_satisfied("blocker", &graph, None));
    }

    #[test]
    fn is_blocker_satisfied_local_missing_treated_as_unblocked() {
        let graph = WorkGraph::new();
        assert!(is_blocker_satisfied("nonexistent", &graph, None));
    }

    #[test]
    fn is_blocker_satisfied_remote_ref_without_dir() {
        let graph = WorkGraph::new();
        // Remote ref without workgraph_dir → treated as blocked
        assert!(!is_blocker_satisfied("peer:task-id", &graph, None));
    }
}
