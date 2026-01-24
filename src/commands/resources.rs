use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use workgraph::graph::{Resource, Status, WorkGraph};
use workgraph::parser::load_graph;

use super::graph_path;

/// Resource utilization data
#[derive(Debug, Clone, Serialize)]
pub struct ResourceUtilization {
    pub id: String,
    pub name: Option<String>,
    pub available: f64,
    pub unit: Option<String>,
    /// Cost from open/in-progress tasks
    pub committed: f64,
    /// Cost from done tasks
    pub spent: f64,
    /// available - committed
    pub remaining: f64,
    /// committed / available as percentage
    pub committed_percent: f64,
    /// Whether committed exceeds available
    pub over_budget: bool,
    /// Task IDs at risk (if over budget)
    pub tasks_at_risk: Vec<String>,
}

/// Output structure for JSON
#[derive(Debug, Serialize)]
pub struct ResourcesOutput {
    pub resources: Vec<ResourceUtilization>,
    pub alerts: Vec<ResourceUtilization>,
}

/// Calculate resource utilization from the graph
pub fn calculate_utilization(graph: &WorkGraph) -> Vec<ResourceUtilization> {
    let mut utilizations = Vec::new();

    // Get all resources with available capacity defined
    let resources: Vec<&Resource> = graph
        .resources()
        .filter(|r| r.available.is_some())
        .collect();

    for resource in resources {
        let available = resource.available.unwrap_or(0.0);

        // Find all tasks that require this resource
        let mut committed = 0.0;
        let mut spent = 0.0;
        let mut open_tasks: Vec<String> = Vec::new();

        for task in graph.tasks() {
            if task.requires.contains(&resource.id) {
                let cost = task
                    .estimate
                    .as_ref()
                    .and_then(|e| e.cost)
                    .unwrap_or(0.0);

                match task.status {
                    Status::Open | Status::InProgress | Status::Blocked => {
                        committed += cost;
                        if cost > 0.0 {
                            open_tasks.push(task.id.clone());
                        }
                    }
                    Status::Done => {
                        spent += cost;
                    }
                    Status::Failed | Status::Abandoned => {
                        // Failed/abandoned tasks don't count toward resource usage
                    }
                }
            }
        }

        let remaining = available - committed;
        let committed_percent = if available > 0.0 {
            (committed / available) * 100.0
        } else {
            0.0
        };
        let over_budget = committed > available;

        // Tasks at risk are the open tasks when over budget
        let tasks_at_risk = if over_budget { open_tasks } else { vec![] };

        utilizations.push(ResourceUtilization {
            id: resource.id.clone(),
            name: resource.name.clone(),
            available,
            unit: resource.unit.clone(),
            committed,
            spent,
            remaining,
            committed_percent,
            over_budget,
            tasks_at_risk,
        });
    }

    // Sort by id for consistent output
    utilizations.sort_by(|a, b| a.id.cmp(&b.id));

    utilizations
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let utilizations = calculate_utilization(&graph);

    if utilizations.is_empty() {
        if json {
            let output = ResourcesOutput {
                resources: vec![],
                alerts: vec![],
            };
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("No resources with capacity defined.");
            println!("Add resources with --available to track utilization.");
        }
        return Ok(());
    }

    if json {
        let alerts: Vec<_> = utilizations.iter().filter(|u| u.over_budget).cloned().collect();
        let output = ResourcesOutput {
            resources: utilizations,
            alerts,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_human_output(&utilizations);
    }

    Ok(())
}

fn format_amount(amount: f64, unit: &Option<String>) -> String {
    match unit.as_deref() {
        Some("usd") | Some("USD") | Some("$") => format!("${:.0}", amount),
        Some(u) => format!("{:.0} {}", amount, u),
        None => format!("{:.0}", amount),
    }
}

fn print_human_output(utilizations: &[ResourceUtilization]) {
    println!("Resource Utilization:");
    println!();

    // First print non-alert resources
    for util in utilizations.iter().filter(|u| !u.over_budget) {
        let display_name = util.name.as_deref().unwrap_or(&util.id);
        let available_str = format_amount(util.available, &util.unit);

        println!("  {} ({} available)", display_name, available_str);

        let committed_str = format_amount(util.committed, &util.unit);
        println!(
            "    Committed (open tasks): {} ({:.0}%)",
            committed_str, util.committed_percent
        );

        let spent_str = format_amount(util.spent, &util.unit);
        println!("    Spent (done tasks): {}", spent_str);

        let remaining_str = format_amount(util.remaining, &util.unit);
        println!("    Remaining: {}", remaining_str);

        println!();
    }

    // Then print alerts
    for util in utilizations.iter().filter(|u| u.over_budget) {
        let display_name = util.name.as_deref().unwrap_or(&util.id);
        let available_str = format_amount(util.available, &util.unit);

        println!("  ALERT: {} ({} available)", display_name, available_str);

        let committed_str = format_amount(util.committed, &util.unit);
        println!(
            "    Committed: {} ({:.0}% - OVER BUDGET)",
            committed_str, util.committed_percent
        );

        if !util.tasks_at_risk.is_empty() {
            println!("    Tasks at risk: {}", util.tasks_at_risk.join(", "));
        }

        println!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Estimate, Node, Resource, Status, Task};

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

    fn make_resource(id: &str, available: f64, unit: &str) -> Resource {
        Resource {
            id: id.to_string(),
            name: Some(id.to_string()),
            resource_type: Some("money".to_string()),
            available: Some(available),
            unit: Some(unit.to_string()),
        }
    }

    #[test]
    fn test_calculate_utilization_empty_graph() {
        let graph = WorkGraph::new();
        let util = calculate_utilization(&graph);
        assert!(util.is_empty());
    }

    #[test]
    fn test_calculate_utilization_resource_without_available() {
        let mut graph = WorkGraph::new();
        let resource = Resource {
            id: "budget".to_string(),
            name: Some("Budget".to_string()),
            resource_type: Some("money".to_string()),
            available: None, // No available field
            unit: Some("usd".to_string()),
        };
        graph.add_node(Node::Resource(resource));

        let util = calculate_utilization(&graph);
        assert!(util.is_empty()); // Should be filtered out
    }

    #[test]
    fn test_calculate_utilization_basic() {
        let mut graph = WorkGraph::new();

        // Add a resource
        let resource = make_resource("engineering-budget", 50000.0, "usd");
        graph.add_node(Node::Resource(resource));

        // Add open task requiring the resource
        let mut task1 = make_task("task-1", "Task 1");
        task1.requires = vec!["engineering-budget".to_string()];
        task1.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(32000.0),
        });
        graph.add_node(Node::Task(task1));

        // Add done task requiring the resource
        let mut task2 = make_task("task-2", "Task 2");
        task2.status = Status::Done;
        task2.requires = vec!["engineering-budget".to_string()];
        task2.estimate = Some(Estimate {
            hours: Some(5.0),
            cost: Some(18000.0),
        });
        graph.add_node(Node::Task(task2));

        let util = calculate_utilization(&graph);
        assert_eq!(util.len(), 1);

        let u = &util[0];
        assert_eq!(u.id, "engineering-budget");
        assert_eq!(u.available, 50000.0);
        assert_eq!(u.committed, 32000.0);
        assert_eq!(u.spent, 18000.0);
        assert_eq!(u.remaining, 18000.0);
        assert!((u.committed_percent - 64.0).abs() < 0.01);
        assert!(!u.over_budget);
    }

    #[test]
    fn test_calculate_utilization_over_budget() {
        let mut graph = WorkGraph::new();

        // Add a resource with small capacity
        let resource = make_resource("design-budget", 5000.0, "usd");
        graph.add_node(Node::Resource(resource));

        // Add tasks that exceed the budget
        let mut task1 = make_task("logo-redesign", "Logo Redesign");
        task1.requires = vec!["design-budget".to_string()];
        task1.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(4000.0),
        });
        graph.add_node(Node::Task(task1));

        let mut task2 = make_task("ui-mockups", "UI Mockups");
        task2.requires = vec!["design-budget".to_string()];
        task2.estimate = Some(Estimate {
            hours: Some(5.0),
            cost: Some(2500.0),
        });
        graph.add_node(Node::Task(task2));

        let util = calculate_utilization(&graph);
        assert_eq!(util.len(), 1);

        let u = &util[0];
        assert_eq!(u.id, "design-budget");
        assert_eq!(u.available, 5000.0);
        assert_eq!(u.committed, 6500.0);
        assert!(u.over_budget);
        assert!((u.committed_percent - 130.0).abs() < 0.01);
        assert_eq!(u.tasks_at_risk.len(), 2);
        assert!(u.tasks_at_risk.contains(&"logo-redesign".to_string()));
        assert!(u.tasks_at_risk.contains(&"ui-mockups".to_string()));
    }

    #[test]
    fn test_calculate_utilization_in_progress_counts_as_committed() {
        let mut graph = WorkGraph::new();

        let resource = make_resource("budget", 1000.0, "usd");
        graph.add_node(Node::Resource(resource));

        let mut task = make_task("task-1", "Task 1");
        task.status = Status::InProgress;
        task.requires = vec!["budget".to_string()];
        task.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(500.0),
        });
        graph.add_node(Node::Task(task));

        let util = calculate_utilization(&graph);
        let u = &util[0];
        assert_eq!(u.committed, 500.0);
        assert_eq!(u.spent, 0.0);
    }

    #[test]
    fn test_calculate_utilization_blocked_counts_as_committed() {
        let mut graph = WorkGraph::new();

        let resource = make_resource("budget", 1000.0, "usd");
        graph.add_node(Node::Resource(resource));

        let mut task = make_task("task-1", "Task 1");
        task.status = Status::Blocked;
        task.requires = vec!["budget".to_string()];
        task.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(500.0),
        });
        graph.add_node(Node::Task(task));

        let util = calculate_utilization(&graph);
        let u = &util[0];
        assert_eq!(u.committed, 500.0);
        assert_eq!(u.spent, 0.0);
    }

    #[test]
    fn test_calculate_utilization_task_without_cost() {
        let mut graph = WorkGraph::new();

        let resource = make_resource("budget", 1000.0, "usd");
        graph.add_node(Node::Resource(resource));

        let mut task = make_task("task-1", "Task 1");
        task.requires = vec!["budget".to_string()];
        // No estimate set
        graph.add_node(Node::Task(task));

        let util = calculate_utilization(&graph);
        let u = &util[0];
        assert_eq!(u.committed, 0.0);
        assert_eq!(u.spent, 0.0);
        assert!(!u.over_budget);
        // Task without cost should not be in tasks_at_risk
        assert!(u.tasks_at_risk.is_empty());
    }

    #[test]
    fn test_calculate_utilization_multiple_resources() {
        let mut graph = WorkGraph::new();

        let resource1 = make_resource("budget-a", 1000.0, "usd");
        let resource2 = make_resource("budget-b", 2000.0, "usd");
        graph.add_node(Node::Resource(resource1));
        graph.add_node(Node::Resource(resource2));

        let mut task1 = make_task("task-1", "Task 1");
        task1.requires = vec!["budget-a".to_string()];
        task1.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(500.0),
        });
        graph.add_node(Node::Task(task1));

        let mut task2 = make_task("task-2", "Task 2");
        task2.requires = vec!["budget-b".to_string()];
        task2.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(1500.0),
        });
        graph.add_node(Node::Task(task2));

        let util = calculate_utilization(&graph);
        assert_eq!(util.len(), 2);

        // Results should be sorted by id
        assert_eq!(util[0].id, "budget-a");
        assert_eq!(util[0].committed, 500.0);
        assert_eq!(util[1].id, "budget-b");
        assert_eq!(util[1].committed, 1500.0);
    }

    #[test]
    fn test_format_amount_usd() {
        let unit = Some("usd".to_string());
        assert_eq!(format_amount(1000.0, &unit), "$1000");
    }

    #[test]
    fn test_format_amount_other_unit() {
        let unit = Some("hours".to_string());
        assert_eq!(format_amount(40.0, &unit), "40 hours");
    }

    #[test]
    fn test_format_amount_no_unit() {
        let unit: Option<String> = None;
        assert_eq!(format_amount(100.0, &unit), "100");
    }

    #[test]
    fn test_json_output_structure() {
        let util = ResourceUtilization {
            id: "budget".to_string(),
            name: Some("Budget".to_string()),
            available: 1000.0,
            unit: Some("usd".to_string()),
            committed: 500.0,
            spent: 200.0,
            remaining: 500.0,
            committed_percent: 50.0,
            over_budget: false,
            tasks_at_risk: vec![],
        };

        let output = ResourcesOutput {
            resources: vec![util.clone()],
            alerts: vec![],
        };

        let json = serde_json::to_string_pretty(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["resources"][0]["id"], "budget");
        assert_eq!(parsed["resources"][0]["available"], 1000.0);
        assert_eq!(parsed["resources"][0]["committed"], 500.0);
        assert_eq!(parsed["alerts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_json_output_with_alerts() {
        let util = ResourceUtilization {
            id: "budget".to_string(),
            name: Some("Budget".to_string()),
            available: 1000.0,
            unit: Some("usd".to_string()),
            committed: 1500.0,
            spent: 0.0,
            remaining: -500.0,
            committed_percent: 150.0,
            over_budget: true,
            tasks_at_risk: vec!["task-1".to_string()],
        };

        let output = ResourcesOutput {
            resources: vec![util.clone()],
            alerts: vec![util],
        };

        let json = serde_json::to_string_pretty(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["alerts"].as_array().unwrap().len(), 1);
        assert_eq!(parsed["alerts"][0]["over_budget"], true);
    }
}
