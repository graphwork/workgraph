use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use workgraph::agency;
use workgraph::graph::Status;
use workgraph::parser::load_graph;
use workgraph::query::ready_tasks;

use super::graph_path;

/// Information about an agent's workload
#[derive(Debug, Serialize)]
struct AgentWorkload {
    id: String,
    name: String,
    assigned_count: usize,
    assigned_hours: f64,
    in_progress_count: usize,
    capacity: Option<f64>,
    load_percent: Option<f64>,
    is_overloaded: bool,
}

/// JSON output structure
#[derive(Debug, Serialize)]
struct WorkloadOutput {
    agents: Vec<AgentWorkload>,
    unassigned_count: usize,
    ready_unassigned_count: usize,
}

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Get ready tasks for later calculation
    let ready = ready_tasks(&graph);
    let ready_ids: std::collections::HashSet<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    // Build agent workload map
    let mut agent_workloads: HashMap<String, AgentWorkload> = HashMap::new();

    // Initialize with known agents from the agency directory
    let agents_dir = dir.join("agency").join("agents");
    if let Ok(agents) = agency::load_all_agents(&agents_dir) {
        for agent in agents {
            agent_workloads.insert(
                agent.id.clone(),
                AgentWorkload {
                    id: agent.id.clone(),
                    name: agent.name.clone(),
                    assigned_count: 0,
                    assigned_hours: 0.0,
                    in_progress_count: 0,
                    capacity: agent.capacity,
                    load_percent: None,
                    is_overloaded: false,
                },
            );
        }
    }

    let mut unassigned_count = 0;
    let mut ready_unassigned_count = 0;

    // Process tasks
    for task in graph.tasks() {
        // Only count open and in-progress tasks
        if task.status == Status::Done {
            continue;
        }

        match &task.assigned {
            Some(agent_id) => {
                // Get or create agent entry
                let workload = agent_workloads.entry(agent_id.clone()).or_insert_with(|| {
                    AgentWorkload {
                        id: agent_id.clone(),
                        name: agent_id.clone(),
                        assigned_count: 0,
                        assigned_hours: 0.0,
                        in_progress_count: 0,
                        capacity: None,
                        load_percent: None,
                        is_overloaded: false,
                    }
                });

                workload.assigned_count += 1;

                // Add estimated hours
                if let Some(ref estimate) = task.estimate {
                    if let Some(hours) = estimate.hours {
                        workload.assigned_hours += hours;
                    }
                }

                // Count in-progress tasks
                if task.status == Status::InProgress {
                    workload.in_progress_count += 1;
                }
            }
            None => {
                unassigned_count += 1;
                if ready_ids.contains(task.id.as_str()) {
                    ready_unassigned_count += 1;
                }
            }
        }
    }

    // Calculate load percentages
    for workload in agent_workloads.values_mut() {
        if let Some(capacity) = workload.capacity {
            if capacity > 0.0 {
                let load = (workload.assigned_hours / capacity) * 100.0;
                workload.load_percent = Some(load);
                workload.is_overloaded = load > 100.0;
            }
        }
    }

    // Sort agents by id for consistent output
    let mut agents: Vec<AgentWorkload> = agent_workloads.into_values().collect();
    agents.sort_by(|a, b| a.id.cmp(&b.id));

    if json {
        let output = WorkloadOutput {
            agents,
            unassigned_count,
            ready_unassigned_count,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_human_readable(&agents, unassigned_count, ready_unassigned_count);
    }

    Ok(())
}

fn print_human_readable(
    agents: &[AgentWorkload],
    unassigned_count: usize,
    ready_unassigned_count: usize,
) {
    if agents.is_empty() && unassigned_count == 0 {
        println!("No tasks or agents found.");
        return;
    }

    println!("Agent Workload (open + in-progress tasks):\n");

    if agents.is_empty() {
        println!("  No agents defined.");
    } else {
        for agent in agents {
            println!("  {} ({})", agent.name, agency::short_hash(&agent.id));

            // Assigned tasks and hours
            let hours_str = format!("{:.0}h estimated", agent.assigned_hours);
            println!(
                "    Assigned: {} task{} ({})",
                agent.assigned_count,
                if agent.assigned_count == 1 { "" } else { "s" },
                hours_str
            );

            // In-progress count
            println!(
                "    In progress: {} task{}",
                agent.in_progress_count,
                if agent.in_progress_count == 1 { "" } else { "s" }
            );

            // Capacity
            if let Some(capacity) = agent.capacity {
                println!("    Capacity: {:.0}h/week", capacity);
            }

            // Load percentage
            if let Some(load) = agent.load_percent {
                if agent.is_overloaded {
                    println!("    Load: {:.0}% [WARNING: overloaded]", load);
                } else {
                    println!("    Load: {:.0}%", load);
                }
            } else if agent.capacity.is_none() {
                // No capacity set - can't calculate load
                println!("    Load: N/A (no capacity set)");
            }

            println!();
        }
    }

    // Unassigned tasks
    println!("Unassigned tasks: {}", unassigned_count);
    if ready_unassigned_count > 0 {
        println!(
            "  Ready & unassigned: {} (potential parallelization)",
            ready_unassigned_count
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::agency::{Agent, Lineage, PerformanceRecord};
    use workgraph::graph::{Estimate, Node, Task, TrustLevel, WorkGraph};

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
            model: None,
            verify: None,
            agent: None,
        }
    }

    fn make_agent(id: &str, name: &str, capacity: Option<f64>) -> Agent {
        Agent {
            id: id.to_string(),
            role_id: String::new(),
            motivation_id: String::new(),
            name: name.to_string(),
            performance: PerformanceRecord {
                task_count: 0,
                avg_score: None,
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            capabilities: vec![],
            rate: None,
            capacity,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".to_string(),
        }
    }

    /// Build an AgentWorkload map from a slice of Agents (mirrors run() logic)
    fn build_agent_workloads(agents: &[Agent]) -> HashMap<String, AgentWorkload> {
        let mut map = HashMap::new();
        for agent in agents {
            map.insert(
                agent.id.clone(),
                AgentWorkload {
                    id: agent.id.clone(),
                    name: agent.name.clone(),
                    assigned_count: 0,
                    assigned_hours: 0.0,
                    in_progress_count: 0,
                    capacity: agent.capacity,
                    load_percent: None,
                    is_overloaded: false,
                },
            );
        }
        map
    }

    /// Process tasks into an agent workload map (mirrors run() logic)
    fn process_tasks(
        graph: &WorkGraph,
        workloads: &mut HashMap<String, AgentWorkload>,
    ) {
        for task in graph.tasks() {
            if task.status == Status::Done {
                continue;
            }
            if let Some(agent_id) = &task.assigned {
                let workload = workloads.entry(agent_id.clone()).or_insert_with(|| {
                    AgentWorkload {
                        id: agent_id.clone(),
                        name: agent_id.clone(),
                        assigned_count: 0,
                        assigned_hours: 0.0,
                        in_progress_count: 0,
                        capacity: None,
                        load_percent: None,
                        is_overloaded: false,
                    }
                });
                workload.assigned_count += 1;
                if let Some(ref estimate) = task.estimate {
                    if let Some(hours) = estimate.hours {
                        workload.assigned_hours += hours;
                    }
                }
                if task.status == Status::InProgress {
                    workload.in_progress_count += 1;
                }
            }
        }
    }

    /// Calculate load percentages for all workloads (mirrors run() logic)
    fn calculate_loads(workloads: &mut HashMap<String, AgentWorkload>) {
        for workload in workloads.values_mut() {
            if let Some(capacity) = workload.capacity {
                if capacity > 0.0 {
                    let load = (workload.assigned_hours / capacity) * 100.0;
                    workload.load_percent = Some(load);
                    workload.is_overloaded = load > 100.0;
                }
            }
        }
    }

    #[test]
    fn test_empty_graph() {
        let graph = WorkGraph::new();
        let ready = ready_tasks(&graph);
        let ready_ids: std::collections::HashSet<&str> =
            ready.iter().map(|t| t.id.as_str()).collect();

        let agent_workloads: HashMap<String, AgentWorkload> = HashMap::new();
        let mut unassigned_count = 0;
        let mut ready_unassigned_count = 0;

        for task in graph.tasks() {
            if task.status == Status::Done {
                continue;
            }
            match &task.assigned {
                Some(_) => {}
                None => {
                    unassigned_count += 1;
                    if ready_ids.contains(task.id.as_str()) {
                        ready_unassigned_count += 1;
                    }
                }
            }
        }

        assert!(agent_workloads.is_empty());
        assert_eq!(unassigned_count, 0);
        assert_eq!(ready_unassigned_count, 0);
    }

    #[test]
    fn test_single_agent_single_task() {
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Task 1");
        task.assigned = Some("alice".to_string());
        task.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });
        graph.add_node(Node::Task(task));

        let agents = vec![make_agent("alice", "Alice", Some(40.0))];
        let mut workloads = build_agent_workloads(&agents);
        process_tasks(&graph, &mut workloads);
        calculate_loads(&mut workloads);

        let alice = workloads.get("alice").unwrap();
        assert_eq!(alice.assigned_count, 1);
        assert_eq!(alice.assigned_hours, 8.0);
        assert_eq!(alice.load_percent, Some(20.0));
        assert!(!alice.is_overloaded);
    }

    #[test]
    fn test_overloaded_agent() {
        let mut graph = WorkGraph::new();

        // Add tasks totaling 50 hours (over 40h capacity)
        for i in 1..=5 {
            let mut task = make_task(&format!("t{}", i), &format!("Task {}", i));
            task.assigned = Some("bob".to_string());
            task.estimate = Some(Estimate {
                hours: Some(10.0),
                cost: None,
            });
            graph.add_node(Node::Task(task));
        }

        let agents = vec![make_agent("bob", "Bob", Some(40.0))];
        let mut workloads = build_agent_workloads(&agents);
        process_tasks(&graph, &mut workloads);
        calculate_loads(&mut workloads);

        let bob = workloads.get("bob").unwrap();
        assert_eq!(bob.assigned_count, 5);
        assert_eq!(bob.assigned_hours, 50.0);
        assert_eq!(bob.load_percent, Some(125.0));
        assert!(bob.is_overloaded);
    }

    #[test]
    fn test_unassigned_tasks() {
        let mut graph = WorkGraph::new();

        // Add some unassigned tasks
        let t1 = make_task("t1", "Task 1");
        let t2 = make_task("t2", "Task 2");
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let ready = ready_tasks(&graph);
        let ready_ids: std::collections::HashSet<&str> =
            ready.iter().map(|t| t.id.as_str()).collect();

        let mut unassigned_count = 0;
        let mut ready_unassigned_count = 0;

        for task in graph.tasks() {
            if task.status == Status::Done {
                continue;
            }
            if task.assigned.is_none() {
                unassigned_count += 1;
                if ready_ids.contains(task.id.as_str()) {
                    ready_unassigned_count += 1;
                }
            }
        }

        assert_eq!(unassigned_count, 2);
        assert_eq!(ready_unassigned_count, 2);
    }

    #[test]
    fn test_in_progress_tasks_counted() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.assigned = Some("alice".to_string());
        t1.status = Status::InProgress;
        t1.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });

        let mut t2 = make_task("t2", "Task 2");
        t2.assigned = Some("alice".to_string());
        t2.status = Status::Open;
        t2.estimate = Some(Estimate {
            hours: Some(4.0),
            cost: None,
        });

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let agents = vec![make_agent("alice", "Alice", Some(40.0))];
        let mut workloads = build_agent_workloads(&agents);
        process_tasks(&graph, &mut workloads);

        let alice = workloads.get("alice").unwrap();
        assert_eq!(alice.assigned_count, 2);
        assert_eq!(alice.assigned_hours, 12.0);
        assert_eq!(alice.in_progress_count, 1);
    }

    #[test]
    fn test_done_tasks_not_counted() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.assigned = Some("alice".to_string());
        t1.status = Status::Done;
        t1.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });

        graph.add_node(Node::Task(t1));

        let agents = vec![make_agent("alice", "Alice", Some(40.0))];
        let mut workloads = build_agent_workloads(&agents);
        process_tasks(&graph, &mut workloads);

        let alice = workloads.get("alice").unwrap();
        assert_eq!(alice.assigned_count, 0);
        assert_eq!(alice.assigned_hours, 0.0);
    }

    #[test]
    fn test_agent_without_capacity() {
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Task 1");
        task.assigned = Some("claude-agent".to_string());
        task.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });
        graph.add_node(Node::Task(task));

        let agents = vec![make_agent("claude-agent", "Claude Agent", None)];
        let mut workloads = build_agent_workloads(&agents);
        process_tasks(&graph, &mut workloads);
        calculate_loads(&mut workloads);

        let agent = workloads.get("claude-agent").unwrap();
        assert_eq!(agent.assigned_count, 1);
        assert_eq!(agent.assigned_hours, 8.0);
        assert!(agent.capacity.is_none());
        assert!(agent.load_percent.is_none());
        assert!(!agent.is_overloaded);
    }

    #[test]
    fn test_implicit_agent_from_assignment() {
        let mut graph = WorkGraph::new();

        // Task assigned to agent not explicitly defined
        let mut task = make_task("t1", "Task 1");
        task.assigned = Some("implicit-agent".to_string());
        task.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });
        graph.add_node(Node::Task(task));

        // No known agents
        let mut workloads: HashMap<String, AgentWorkload> = HashMap::new();
        process_tasks(&graph, &mut workloads);

        assert!(workloads.contains_key("implicit-agent"));
        let implicit = workloads.get("implicit-agent").unwrap();
        assert_eq!(implicit.assigned_count, 1);
        assert_eq!(implicit.assigned_hours, 8.0);
        assert_eq!(implicit.name, "implicit-agent");
    }
}
