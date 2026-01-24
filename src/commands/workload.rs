use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use workgraph::graph::Status;
use workgraph::parser::load_graph;
use workgraph::query::ready_tasks;

use super::graph_path;

/// Information about an actor's workload
#[derive(Debug, Serialize)]
struct ActorWorkload {
    id: String,
    name: Option<String>,
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
    actors: Vec<ActorWorkload>,
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

    // Build actor workload map
    let mut actor_workloads: HashMap<String, ActorWorkload> = HashMap::new();

    // Initialize with known actors from the graph
    for actor in graph.actors() {
        actor_workloads.insert(
            actor.id.clone(),
            ActorWorkload {
                id: actor.id.clone(),
                name: actor.name.clone(),
                assigned_count: 0,
                assigned_hours: 0.0,
                in_progress_count: 0,
                capacity: actor.capacity,
                load_percent: None,
                is_overloaded: false,
            },
        );
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
            Some(actor_id) => {
                // Get or create actor entry
                let workload = actor_workloads.entry(actor_id.clone()).or_insert_with(|| {
                    ActorWorkload {
                        id: actor_id.clone(),
                        name: None,
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
    for workload in actor_workloads.values_mut() {
        if let Some(capacity) = workload.capacity {
            if capacity > 0.0 {
                let load = (workload.assigned_hours / capacity) * 100.0;
                workload.load_percent = Some(load);
                workload.is_overloaded = load > 100.0;
            }
        }
    }

    // Sort actors by id for consistent output
    let mut actors: Vec<ActorWorkload> = actor_workloads.into_values().collect();
    actors.sort_by(|a, b| a.id.cmp(&b.id));

    if json {
        let output = WorkloadOutput {
            actors,
            unassigned_count,
            ready_unassigned_count,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_human_readable(&actors, unassigned_count, ready_unassigned_count);
    }

    Ok(())
}

fn print_human_readable(
    actors: &[ActorWorkload],
    unassigned_count: usize,
    ready_unassigned_count: usize,
) {
    if actors.is_empty() && unassigned_count == 0 {
        println!("No tasks or actors found.");
        return;
    }

    println!("Actor Workload (open + in-progress tasks):\n");

    if actors.is_empty() {
        println!("  No actors defined.");
    } else {
        for actor in actors {
            // Display name with @ prefix
            let display_name = if actor.id.starts_with('@') {
                actor.id.clone()
            } else {
                format!("@{}", actor.id)
            };
            println!("  {}", display_name);

            // Assigned tasks and hours
            let hours_str = format!("{:.0}h estimated", actor.assigned_hours);
            println!(
                "    Assigned: {} task{} ({})",
                actor.assigned_count,
                if actor.assigned_count == 1 { "" } else { "s" },
                hours_str
            );

            // In-progress count
            println!(
                "    In progress: {} task{}",
                actor.in_progress_count,
                if actor.in_progress_count == 1 { "" } else { "s" }
            );

            // Capacity
            if let Some(capacity) = actor.capacity {
                println!("    Capacity: {:.0}h/week", capacity);
            }

            // Load percentage
            if let Some(load) = actor.load_percent {
                if actor.is_overloaded {
                    println!("    Load: {:.0}% [WARNING: overloaded]", load);
                } else {
                    println!("    Load: {:.0}%", load);
                }
            } else if actor.capacity.is_none() {
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
    use workgraph::graph::{Actor, Estimate, Node, Task, TrustLevel, WorkGraph};

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

    fn make_actor(id: &str, capacity: Option<f64>) -> Actor {
        Actor {
            id: id.to_string(),
            name: Some(format!("{} Name", id)),
            role: None,
            rate: None,
            capacity,
            capabilities: vec![],
            context_limit: None,
            trust_level: TrustLevel::Provisional,
            last_seen: None,
        }
    }

    #[test]
    fn test_empty_graph() {
        let graph = WorkGraph::new();
        let ready = ready_tasks(&graph);
        let ready_ids: std::collections::HashSet<&str> =
            ready.iter().map(|t| t.id.as_str()).collect();

        let actor_workloads: HashMap<String, ActorWorkload> = HashMap::new();
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

        assert!(actor_workloads.is_empty());
        assert_eq!(unassigned_count, 0);
        assert_eq!(ready_unassigned_count, 0);
    }

    #[test]
    fn test_single_actor_single_task() {
        let mut graph = WorkGraph::new();

        let actor = make_actor("alice", Some(40.0));
        graph.add_node(Node::Actor(actor));

        let mut task = make_task("t1", "Task 1");
        task.assigned = Some("alice".to_string());
        task.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });
        graph.add_node(Node::Task(task));

        // Simulate the workload calculation
        let mut actor_workloads: HashMap<String, ActorWorkload> = HashMap::new();
        for actor in graph.actors() {
            actor_workloads.insert(
                actor.id.clone(),
                ActorWorkload {
                    id: actor.id.clone(),
                    name: actor.name.clone(),
                    assigned_count: 0,
                    assigned_hours: 0.0,
                    in_progress_count: 0,
                    capacity: actor.capacity,
                    load_percent: None,
                    is_overloaded: false,
                },
            );
        }

        for task in graph.tasks() {
            if task.status == Status::Done {
                continue;
            }
            if let Some(actor_id) = &task.assigned {
                let workload = actor_workloads.get_mut(actor_id).unwrap();
                workload.assigned_count += 1;
                if let Some(ref estimate) = task.estimate {
                    if let Some(hours) = estimate.hours {
                        workload.assigned_hours += hours;
                    }
                }
            }
        }

        // Calculate load
        for workload in actor_workloads.values_mut() {
            if let Some(capacity) = workload.capacity {
                if capacity > 0.0 {
                    let load = (workload.assigned_hours / capacity) * 100.0;
                    workload.load_percent = Some(load);
                    workload.is_overloaded = load > 100.0;
                }
            }
        }

        let alice = actor_workloads.get("alice").unwrap();
        assert_eq!(alice.assigned_count, 1);
        assert_eq!(alice.assigned_hours, 8.0);
        assert_eq!(alice.load_percent, Some(20.0));
        assert!(!alice.is_overloaded);
    }

    #[test]
    fn test_overloaded_actor() {
        let mut graph = WorkGraph::new();

        let actor = make_actor("bob", Some(40.0));
        graph.add_node(Node::Actor(actor));

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

        let mut actor_workloads: HashMap<String, ActorWorkload> = HashMap::new();
        for actor in graph.actors() {
            actor_workloads.insert(
                actor.id.clone(),
                ActorWorkload {
                    id: actor.id.clone(),
                    name: actor.name.clone(),
                    assigned_count: 0,
                    assigned_hours: 0.0,
                    in_progress_count: 0,
                    capacity: actor.capacity,
                    load_percent: None,
                    is_overloaded: false,
                },
            );
        }

        for task in graph.tasks() {
            if task.status == Status::Done {
                continue;
            }
            if let Some(actor_id) = &task.assigned {
                let workload = actor_workloads.get_mut(actor_id).unwrap();
                workload.assigned_count += 1;
                if let Some(ref estimate) = task.estimate {
                    if let Some(hours) = estimate.hours {
                        workload.assigned_hours += hours;
                    }
                }
            }
        }

        for workload in actor_workloads.values_mut() {
            if let Some(capacity) = workload.capacity {
                if capacity > 0.0 {
                    let load = (workload.assigned_hours / capacity) * 100.0;
                    workload.load_percent = Some(load);
                    workload.is_overloaded = load > 100.0;
                }
            }
        }

        let bob = actor_workloads.get("bob").unwrap();
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

        let actor = make_actor("alice", Some(40.0));
        graph.add_node(Node::Actor(actor));

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

        let mut actor_workloads: HashMap<String, ActorWorkload> = HashMap::new();
        for actor in graph.actors() {
            actor_workloads.insert(
                actor.id.clone(),
                ActorWorkload {
                    id: actor.id.clone(),
                    name: actor.name.clone(),
                    assigned_count: 0,
                    assigned_hours: 0.0,
                    in_progress_count: 0,
                    capacity: actor.capacity,
                    load_percent: None,
                    is_overloaded: false,
                },
            );
        }

        for task in graph.tasks() {
            if task.status == Status::Done {
                continue;
            }
            if let Some(actor_id) = &task.assigned {
                let workload = actor_workloads.get_mut(actor_id).unwrap();
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

        let alice = actor_workloads.get("alice").unwrap();
        assert_eq!(alice.assigned_count, 2);
        assert_eq!(alice.assigned_hours, 12.0);
        assert_eq!(alice.in_progress_count, 1);
    }

    #[test]
    fn test_done_tasks_not_counted() {
        let mut graph = WorkGraph::new();

        let actor = make_actor("alice", Some(40.0));
        graph.add_node(Node::Actor(actor));

        let mut t1 = make_task("t1", "Task 1");
        t1.assigned = Some("alice".to_string());
        t1.status = Status::Done;
        t1.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });

        graph.add_node(Node::Task(t1));

        let mut actor_workloads: HashMap<String, ActorWorkload> = HashMap::new();
        for actor in graph.actors() {
            actor_workloads.insert(
                actor.id.clone(),
                ActorWorkload {
                    id: actor.id.clone(),
                    name: actor.name.clone(),
                    assigned_count: 0,
                    assigned_hours: 0.0,
                    in_progress_count: 0,
                    capacity: actor.capacity,
                    load_percent: None,
                    is_overloaded: false,
                },
            );
        }

        for task in graph.tasks() {
            if task.status == Status::Done {
                continue;
            }
            if let Some(actor_id) = &task.assigned {
                let workload = actor_workloads.get_mut(actor_id).unwrap();
                workload.assigned_count += 1;
            }
        }

        let alice = actor_workloads.get("alice").unwrap();
        assert_eq!(alice.assigned_count, 0);
        assert_eq!(alice.assigned_hours, 0.0);
    }

    #[test]
    fn test_actor_without_capacity() {
        let mut graph = WorkGraph::new();

        // Actor without capacity (like an agent)
        let actor = Actor {
            id: "claude-agent".to_string(),
            name: Some("Claude Agent".to_string()),
            role: Some("agent".to_string()),
            rate: None,
            capacity: None,
            capabilities: vec![],
            context_limit: None,
            trust_level: TrustLevel::Provisional,
            last_seen: None,
        };
        graph.add_node(Node::Actor(actor));

        let mut task = make_task("t1", "Task 1");
        task.assigned = Some("claude-agent".to_string());
        task.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });
        graph.add_node(Node::Task(task));

        let mut actor_workloads: HashMap<String, ActorWorkload> = HashMap::new();
        for actor in graph.actors() {
            actor_workloads.insert(
                actor.id.clone(),
                ActorWorkload {
                    id: actor.id.clone(),
                    name: actor.name.clone(),
                    assigned_count: 0,
                    assigned_hours: 0.0,
                    in_progress_count: 0,
                    capacity: actor.capacity,
                    load_percent: None,
                    is_overloaded: false,
                },
            );
        }

        for task in graph.tasks() {
            if task.status == Status::Done {
                continue;
            }
            if let Some(actor_id) = &task.assigned {
                let workload = actor_workloads.get_mut(actor_id).unwrap();
                workload.assigned_count += 1;
                if let Some(ref estimate) = task.estimate {
                    if let Some(hours) = estimate.hours {
                        workload.assigned_hours += hours;
                    }
                }
            }
        }

        let agent = actor_workloads.get("claude-agent").unwrap();
        assert_eq!(agent.assigned_count, 1);
        assert_eq!(agent.assigned_hours, 8.0);
        assert!(agent.capacity.is_none());
        assert!(agent.load_percent.is_none());
        assert!(!agent.is_overloaded);
    }

    #[test]
    fn test_implicit_actor_from_assignment() {
        let mut graph = WorkGraph::new();

        // Task assigned to actor not explicitly defined
        let mut task = make_task("t1", "Task 1");
        task.assigned = Some("implicit-actor".to_string());
        task.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });
        graph.add_node(Node::Task(task));

        let mut actor_workloads: HashMap<String, ActorWorkload> = HashMap::new();

        // Initialize with known actors
        for actor in graph.actors() {
            actor_workloads.insert(
                actor.id.clone(),
                ActorWorkload {
                    id: actor.id.clone(),
                    name: actor.name.clone(),
                    assigned_count: 0,
                    assigned_hours: 0.0,
                    in_progress_count: 0,
                    capacity: actor.capacity,
                    load_percent: None,
                    is_overloaded: false,
                },
            );
        }

        for task in graph.tasks() {
            if task.status == Status::Done {
                continue;
            }
            if let Some(actor_id) = &task.assigned {
                let workload = actor_workloads.entry(actor_id.clone()).or_insert_with(|| {
                    ActorWorkload {
                        id: actor_id.clone(),
                        name: None,
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
            }
        }

        assert!(actor_workloads.contains_key("implicit-actor"));
        let implicit = actor_workloads.get("implicit-actor").unwrap();
        assert_eq!(implicit.assigned_count, 1);
        assert_eq!(implicit.assigned_hours, 8.0);
        assert!(implicit.name.is_none());
    }
}
