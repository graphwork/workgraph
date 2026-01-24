use anyhow::{Context, Result};
use std::path::Path;
use workgraph::graph::{Status, Task};
use workgraph::parser::load_graph;
use workgraph::query::ready_tasks;

use super::graph_path;

/// Coordination status for JSON output
#[derive(Debug, serde::Serialize)]
pub struct CoordinationStatus {
    pub ready: Vec<TaskSummary>,
    pub in_progress: Vec<TaskSummary>,
    pub blocked: Vec<BlockedTaskSummary>,
    pub done_count: usize,
    pub total_count: usize,
}

#[derive(Debug, serde::Serialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimate: Option<EstimateSummary>,
}

#[derive(Debug, serde::Serialize)]
pub struct EstimateSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
}

#[derive(Debug, serde::Serialize)]
pub struct BlockedTaskSummary {
    pub id: String,
    pub title: String,
    pub blocked_by: Vec<String>,
}

impl TaskSummary {
    fn from_task(task: &Task) -> Self {
        TaskSummary {
            id: task.id.clone(),
            title: task.title.clone(),
            assigned: task.assigned.clone(),
            estimate: task.estimate.as_ref().map(|e| EstimateSummary {
                hours: e.hours,
                cost: e.cost,
            }),
        }
    }
}

impl BlockedTaskSummary {
    fn from_task(task: &Task) -> Self {
        BlockedTaskSummary {
            id: task.id.clone(),
            title: task.title.clone(),
            blocked_by: task.blocked_by.clone(),
        }
    }
}

/// Get coordination status from the graph
pub fn get_coordination_status(
    graph: &workgraph::graph::WorkGraph,
) -> CoordinationStatus {
    let ready: Vec<_> = ready_tasks(graph)
        .iter()
        .map(|t| TaskSummary::from_task(t))
        .collect();

    let in_progress: Vec<_> = graph
        .tasks()
        .filter(|t| t.status == Status::InProgress)
        .map(TaskSummary::from_task)
        .collect();

    let blocked: Vec<_> = graph
        .tasks()
        .filter(|t| {
            t.status == Status::Open
                && !t.blocked_by.is_empty()
                && t.blocked_by.iter().any(|blocker_id| {
                    graph
                        .get_task(blocker_id)
                        .map(|b| b.status != Status::Done)
                        .unwrap_or(false)
                })
        })
        .map(BlockedTaskSummary::from_task)
        .collect();

    let done_count = graph.tasks().filter(|t| t.status == Status::Done).count();
    let total_count = graph.tasks().count();

    CoordinationStatus {
        ready,
        in_progress,
        blocked,
        done_count,
        total_count,
    }
}

pub fn run(dir: &Path, json: bool, max_parallel: Option<usize>) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let status = get_coordination_status(&graph);

    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_human_status(&status, max_parallel);
    }

    Ok(())
}

fn print_human_status(status: &CoordinationStatus, max_parallel: Option<usize>) {
    let max = max_parallel.unwrap_or(usize::MAX);

    // Progress header
    println!(
        "Progress: {}/{} tasks done\n",
        status.done_count, status.total_count
    );

    // In progress tasks
    if !status.in_progress.is_empty() {
        println!("In Progress ({}):", status.in_progress.len());
        for task in &status.in_progress {
            let assigned = task
                .assigned
                .as_ref()
                .map(|a| format!(" [{}]", a))
                .unwrap_or_default();
            println!("  {} - {}{}", task.id, task.title, assigned);
        }
        println!();
    }

    // Ready tasks
    if status.ready.is_empty() {
        if status.in_progress.is_empty() && status.blocked.is_empty() {
            println!("All tasks complete!");
        } else if !status.in_progress.is_empty() {
            println!("No ready tasks. Waiting for in-progress tasks to complete.");
        } else {
            println!("No ready tasks. All remaining tasks are blocked.");
        }
    } else {
        let display_count = status.ready.len().min(max);
        println!(
            "Ready for parallel execution ({}{} tasks):",
            display_count,
            if status.ready.len() > max {
                format!(" of {}", status.ready.len())
            } else {
                String::new()
            }
        );
        println!();

        for task in status.ready.iter().take(max) {
            println!("Task: {}", task.id);
            println!("  Title: {}", task.title);
            if let Some(estimate) = &task.estimate {
                let mut parts = Vec::new();
                if let Some(h) = estimate.hours {
                    parts.push(format!("{}h", h));
                }
                if let Some(c) = estimate.cost {
                    parts.push(format!("${}", c));
                }
                if !parts.is_empty() {
                    println!("  Estimate: {}", parts.join(", "));
                }
            }
            println!("  To claim: wg claim {}", task.id);
            println!("  When done: wg done {}", task.id);
            println!();
        }

        println!("Run these in parallel, then re-run 'wg coordinate' to see what's next.");
    }

    // Blocked tasks summary
    if !status.blocked.is_empty() {
        println!();
        println!("Blocked ({}):", status.blocked.len());
        for task in &status.blocked {
            println!(
                "  {} - {} (waiting on: {})",
                task.id,
                task.title,
                task.blocked_by.join(", ")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Estimate, Node, WorkGraph};

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
    fn test_coordination_status_empty_graph() {
        let graph = WorkGraph::new();
        let status = get_coordination_status(&graph);

        assert!(status.ready.is_empty());
        assert!(status.in_progress.is_empty());
        assert!(status.blocked.is_empty());
        assert_eq!(status.done_count, 0);
        assert_eq!(status.total_count, 0);
    }

    #[test]
    fn test_coordination_status_single_ready_task() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Task 1")));

        let status = get_coordination_status(&graph);

        assert_eq!(status.ready.len(), 1);
        assert_eq!(status.ready[0].id, "t1");
        assert!(status.in_progress.is_empty());
        assert!(status.blocked.is_empty());
        assert_eq!(status.done_count, 0);
        assert_eq!(status.total_count, 1);
    }

    #[test]
    fn test_coordination_status_with_in_progress() {
        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Task 1");
        task.status = Status::InProgress;
        task.assigned = Some("agent-1".to_string());
        graph.add_node(Node::Task(task));

        let status = get_coordination_status(&graph);

        assert!(status.ready.is_empty());
        assert_eq!(status.in_progress.len(), 1);
        assert_eq!(status.in_progress[0].id, "t1");
        assert_eq!(status.in_progress[0].assigned, Some("agent-1".to_string()));
    }

    #[test]
    fn test_coordination_status_with_blocked() {
        let mut graph = WorkGraph::new();

        let blocker = make_task("blocker", "Blocker task");
        let mut blocked = make_task("blocked", "Blocked task");
        blocked.blocked_by = vec!["blocker".to_string()];

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked));

        let status = get_coordination_status(&graph);

        assert_eq!(status.ready.len(), 1);
        assert_eq!(status.ready[0].id, "blocker");
        assert_eq!(status.blocked.len(), 1);
        assert_eq!(status.blocked[0].id, "blocked");
        assert_eq!(status.blocked[0].blocked_by, vec!["blocker"]);
    }

    #[test]
    fn test_coordination_status_counts_done() {
        let mut graph = WorkGraph::new();

        let mut done = make_task("done", "Done task");
        done.status = Status::Done;

        let open = make_task("open", "Open task");

        graph.add_node(Node::Task(done));
        graph.add_node(Node::Task(open));

        let status = get_coordination_status(&graph);

        assert_eq!(status.done_count, 1);
        assert_eq!(status.total_count, 2);
    }

    #[test]
    fn test_coordination_status_with_estimates() {
        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Task 1");
        task.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: Some(800.0),
        });
        graph.add_node(Node::Task(task));

        let status = get_coordination_status(&graph);

        assert_eq!(status.ready.len(), 1);
        let estimate = status.ready[0].estimate.as_ref().unwrap();
        assert_eq!(estimate.hours, Some(8.0));
        assert_eq!(estimate.cost, Some(800.0));
    }

    #[test]
    fn test_coordination_status_unblocked_when_blocker_done() {
        let mut graph = WorkGraph::new();

        let mut blocker = make_task("blocker", "Blocker task");
        blocker.status = Status::Done;

        let mut blocked = make_task("blocked", "Was blocked task");
        blocked.blocked_by = vec!["blocker".to_string()];

        graph.add_node(Node::Task(blocker));
        graph.add_node(Node::Task(blocked));

        let status = get_coordination_status(&graph);

        // The "blocked" task is now ready since its blocker is done
        assert_eq!(status.ready.len(), 1);
        assert_eq!(status.ready[0].id, "blocked");
        assert!(status.blocked.is_empty());
    }

    #[test]
    fn test_task_summary_from_task() {
        let mut task = make_task("t1", "Task 1");
        task.assigned = Some("agent".to_string());
        task.estimate = Some(Estimate {
            hours: Some(4.0),
            cost: None,
        });

        let summary = TaskSummary::from_task(&task);

        assert_eq!(summary.id, "t1");
        assert_eq!(summary.title, "Task 1");
        assert_eq!(summary.assigned, Some("agent".to_string()));
        assert_eq!(summary.estimate.as_ref().unwrap().hours, Some(4.0));
        assert_eq!(summary.estimate.as_ref().unwrap().cost, None);
    }

    #[test]
    fn test_json_serialization() {
        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Task 1");
        task.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: Some(800.0),
        });
        graph.add_node(Node::Task(task));

        let status = get_coordination_status(&graph);
        let json = serde_json::to_string(&status).unwrap();

        // Verify JSON can be parsed back
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["ready"][0]["id"], "t1");
        assert_eq!(parsed["done_count"], 0);
        assert_eq!(parsed["total_count"], 1);
    }
}
