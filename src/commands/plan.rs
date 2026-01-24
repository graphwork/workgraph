use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use workgraph::parser::load_graph;
use workgraph::query::{project_summary, tasks_within_budget, tasks_within_hours, ProjectSummary};

use super::graph_path;

/// JSON output for plan command
#[derive(Debug, Serialize)]
pub struct PlanOutput {
    pub summary: ProjectSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_plan: Option<BudgetPlanOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hours_plan: Option<HoursPlanOutput>,
}

#[derive(Debug, Serialize)]
pub struct BudgetPlanOutput {
    pub budget: f64,
    pub fits: Vec<TaskPlanItem>,
    pub exceeds: Vec<TaskPlanItem>,
    pub remaining: f64,
}

#[derive(Debug, Serialize)]
pub struct HoursPlanOutput {
    pub hours: f64,
    pub fits: Vec<TaskPlanItem>,
    pub exceeds: Vec<TaskPlanItem>,
    pub remaining: f64,
}

#[derive(Debug, Serialize)]
pub struct TaskPlanItem {
    pub id: String,
    pub title: String,
    pub cost: f64,
    pub hours: f64,
    pub is_ready: bool,
}

pub fn run(dir: &Path, budget: Option<f64>, hours: Option<f64>, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let summary = project_summary(&graph);

    let budget_plan = budget.map(|b| {
        let result = tasks_within_budget(&graph, b);
        BudgetPlanOutput {
            budget: b,
            fits: result
                .fits
                .iter()
                .map(|t| TaskPlanItem {
                    id: t.id.to_string(),
                    title: t.title.to_string(),
                    cost: t.cost,
                    hours: t.hours,
                    is_ready: t.is_ready,
                })
                .collect(),
            exceeds: result
                .exceeds
                .iter()
                .map(|t| TaskPlanItem {
                    id: t.id.to_string(),
                    title: t.title.to_string(),
                    cost: t.cost,
                    hours: t.hours,
                    is_ready: t.is_ready,
                })
                .collect(),
            remaining: result.remaining,
        }
    });

    let hours_plan = hours.map(|h| {
        let result = tasks_within_hours(&graph, h);
        HoursPlanOutput {
            hours: h,
            fits: result
                .fits
                .iter()
                .map(|t| TaskPlanItem {
                    id: t.id.to_string(),
                    title: t.title.to_string(),
                    cost: t.cost,
                    hours: t.hours,
                    is_ready: t.is_ready,
                })
                .collect(),
            exceeds: result
                .exceeds
                .iter()
                .map(|t| TaskPlanItem {
                    id: t.id.to_string(),
                    title: t.title.to_string(),
                    cost: t.cost,
                    hours: t.hours,
                    is_ready: t.is_ready,
                })
                .collect(),
            remaining: result.remaining,
        }
    });

    if json {
        let output = PlanOutput {
            summary,
            budget_plan,
            hours_plan,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_human_output(&summary, budget_plan.as_ref(), hours_plan.as_ref());
    }

    Ok(())
}

fn print_human_output(
    summary: &ProjectSummary,
    budget_plan: Option<&BudgetPlanOutput>,
    hours_plan: Option<&HoursPlanOutput>,
) {
    // If no specific plan requested, show project overview
    if budget_plan.is_none() && hours_plan.is_none() {
        println!("Project Status:");
        println!(
            "  Tasks: {} open, {} done, {} in-progress",
            summary.open, summary.done, summary.in_progress
        );
        println!(
            "  Estimated: ${:.0} / {:.0} hours remaining",
            summary.total_cost, summary.total_hours
        );
        println!("  Ready: {} tasks", summary.ready);
        println!("  Blocked: {} tasks", summary.blocked);
        return;
    }

    // Budget-based planning
    if let Some(plan) = budget_plan {
        println!("With budget of ${:.0}, you can complete:", plan.budget);
        println!();

        if plan.fits.is_empty() {
            println!("  No tasks fit within this budget.");
        } else {
            for task in &plan.fits {
                let status = if task.is_ready { "ready" } else { "unblocked" };
                println!(
                    "  [x] {} (${:.0}, {:.0}h) - {}",
                    task.id, task.cost, task.hours, status
                );
            }
            println!("  Remaining: ${:.0}", plan.remaining);
        }

        if !plan.exceeds.is_empty() {
            println!();
            println!("Cannot fit:");
            for task in &plan.exceeds {
                println!(
                    "  [ ] {} (${:.0}, {:.0}h) - exceeds remaining budget",
                    task.id, task.cost, task.hours
                );
            }
        }
    }

    // Hours-based planning
    if let Some(plan) = hours_plan {
        if budget_plan.is_some() {
            println!();
        }
        println!("With {:.0} hours, you can complete:", plan.hours);
        println!();

        if plan.fits.is_empty() {
            println!("  No tasks fit within this time.");
        } else {
            for task in &plan.fits {
                let status = if task.is_ready { "ready" } else { "unblocked" };
                println!(
                    "  [x] {} ({:.0}h, ${:.0}) - {}",
                    task.id, task.hours, task.cost, status
                );
            }
            println!("  Remaining: {:.0}h", plan.remaining);
        }

        if !plan.exceeds.is_empty() {
            println!();
            println!("Cannot fit:");
            for task in &plan.exceeds {
                println!(
                    "  [ ] {} ({:.0}h, ${:.0}) - exceeds remaining hours",
                    task.id, task.hours, task.cost
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Estimate, Node, Status, Task, WorkGraph};

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
    fn test_budget_plan_output_creation() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.estimate = Some(Estimate {
            hours: Some(4.0),
            cost: Some(400.0),
        });

        graph.add_node(Node::Task(t1));

        let result = tasks_within_budget(&graph, 500.0);
        let plan = BudgetPlanOutput {
            budget: 500.0,
            fits: result
                .fits
                .iter()
                .map(|t| TaskPlanItem {
                    id: t.id.to_string(),
                    title: t.title.to_string(),
                    cost: t.cost,
                    hours: t.hours,
                    is_ready: t.is_ready,
                })
                .collect(),
            exceeds: vec![],
            remaining: result.remaining,
        };

        assert_eq!(plan.budget, 500.0);
        assert_eq!(plan.fits.len(), 1);
        assert_eq!(plan.fits[0].id, "t1");
        assert_eq!(plan.remaining, 100.0);
    }

    #[test]
    fn test_hours_plan_output_creation() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.estimate = Some(Estimate {
            hours: Some(4.0),
            cost: Some(400.0),
        });

        graph.add_node(Node::Task(t1));

        let result = tasks_within_hours(&graph, 10.0);
        let plan = HoursPlanOutput {
            hours: 10.0,
            fits: result
                .fits
                .iter()
                .map(|t| TaskPlanItem {
                    id: t.id.to_string(),
                    title: t.title.to_string(),
                    cost: t.cost,
                    hours: t.hours,
                    is_ready: t.is_ready,
                })
                .collect(),
            exceeds: vec![],
            remaining: result.remaining,
        };

        assert_eq!(plan.hours, 10.0);
        assert_eq!(plan.fits.len(), 1);
        assert_eq!(plan.fits[0].id, "t1");
        assert_eq!(plan.remaining, 6.0);
    }

    #[test]
    fn test_plan_output_serialization() {
        let summary = ProjectSummary {
            open: 2,
            done: 1,
            in_progress: 0,
            ready: 1,
            blocked: 1,
            total_cost: 1000.0,
            total_hours: 10.0,
        };

        let output = PlanOutput {
            summary,
            budget_plan: None,
            hours_plan: None,
        };

        let json = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["summary"]["open"], 2);
        assert_eq!(parsed["summary"]["total_cost"], 1000.0);
    }
}
