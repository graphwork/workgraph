use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::graph::Status;
use workgraph::query::build_reverse_index;

use super::collect_transitive_dependents;

/// Information about the impact of a task
#[derive(Debug, Serialize)]
struct ImpactInfo {
    id: String,
    title: String,
    status: Status,
    hours: Option<f64>,
}

/// JSON output structure
#[derive(Debug, Serialize)]
struct ImpactOutput {
    task: ImpactInfo,
    direct_dependents: Vec<ImpactInfo>,
    transitive_dependents: Vec<ImpactInfo>,
    dependency_chains: Vec<Vec<String>>,
    impact_summary: ImpactSummary,
}

#[derive(Debug, Serialize)]
struct ImpactSummary {
    total_tasks_affected: usize,
    total_hours_at_risk: f64,
}

pub fn run(dir: &Path, id: &str, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    let task = graph.get_task_or_err(id)?;

    // Build reverse index: task_id -> list of tasks that depend on it
    let reverse_index = build_reverse_index(&graph);

    // Find direct dependents
    let direct_ids: Vec<&str> = reverse_index
        .get(id)
        .map(|v| v.iter().map(String::as_str).collect())
        .unwrap_or_default();

    // Find all transitive dependents (excluding direct)
    let mut all_dependents: HashSet<String> = HashSet::new();
    collect_transitive_dependents(&reverse_index, id, &mut all_dependents);

    let transitive_ids: Vec<&str> = all_dependents
        .iter()
        .filter(|tid| !direct_ids.contains(&tid.as_str()))
        .map(String::as_str)
        .collect();

    // Build dependency chains for display
    let chains = build_dependency_chains(&reverse_index, id);

    // Calculate total hours at risk
    let mut total_hours = 0.0;
    for dep_id in &all_dependents {
        if let Some(dep_task) = graph.get_task(dep_id)
            && let Some(ref estimate) = dep_task.estimate
        {
            total_hours += estimate.hours.unwrap_or(0.0);
        }
    }

    // Build output info
    let task_info = ImpactInfo {
        id: task.id.clone(),
        title: task.title.clone(),
        status: task.status,
        hours: task.estimate.as_ref().and_then(|e| e.hours),
    };

    let direct_dependents: Vec<ImpactInfo> = direct_ids
        .iter()
        .filter_map(|tid| graph.get_task(tid))
        .map(|t| ImpactInfo {
            id: t.id.clone(),
            title: t.title.clone(),
            status: t.status,
            hours: t.estimate.as_ref().and_then(|e| e.hours),
        })
        .collect();

    let transitive_dependents: Vec<ImpactInfo> = transitive_ids
        .iter()
        .filter_map(|tid| graph.get_task(tid))
        .map(|t| ImpactInfo {
            id: t.id.clone(),
            title: t.title.clone(),
            status: t.status,
            hours: t.estimate.as_ref().and_then(|e| e.hours),
        })
        .collect();

    if json {
        let output = ImpactOutput {
            task: task_info,
            direct_dependents,
            transitive_dependents,
            dependency_chains: chains,
            impact_summary: ImpactSummary {
                total_tasks_affected: all_dependents.len(),
                total_hours_at_risk: total_hours,
            },
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        // Human-readable output
        println!("Task: {}", task.id);
        println!("Status: {:?}", task.status);
        if let Some(ref est) = task.estimate
            && let Some(hours) = est.hours
        {
            println!("Estimated: {}h", hours);
        }
        println!();

        if direct_dependents.is_empty() {
            println!("Direct dependents: none");
        } else {
            println!("Direct dependents ({}):", direct_dependents.len());
            for dep in &direct_dependents {
                let hours_str = dep.hours.map(|h| format!(" ({}h)", h)).unwrap_or_default();
                println!("  - {}{}", dep.id, hours_str);
            }
        }
        println!();

        if !transitive_dependents.is_empty() {
            println!("Transitive dependents ({}):", transitive_dependents.len());
            for chain in &chains {
                if chain.len() > 1 {
                    println!("  - {}", chain.join(" -> "));
                }
            }
            println!();
        }

        println!("Impact summary:");
        println!("  If {} is delayed by 1 day:", task.id);
        println!("    - {} tasks delayed", all_dependents.len());
        println!("    - Total hours at risk: {}h", total_hours);
    }

    Ok(())
}

/// Build dependency chains for display (BFS to show paths)
fn build_dependency_chains(
    reverse_index: &HashMap<String, Vec<String>>,
    start_id: &str,
) -> Vec<Vec<String>> {
    let mut chains: Vec<Vec<String>> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();

    // Use a queue of (current_id, current_chain)
    let mut queue: Vec<(String, Vec<String>)> = Vec::new();

    if let Some(direct_deps) = reverse_index.get(start_id) {
        for dep_id in direct_deps {
            queue.push((dep_id.clone(), vec![dep_id.clone()]));
        }
    }

    while let Some((current_id, current_chain)) = queue.pop() {
        if let Some(next_deps) = reverse_index.get(&current_id) {
            if next_deps.is_empty() || visited.contains(&current_id) {
                // End of chain
                if current_chain.len() > 1 {
                    chains.push(current_chain);
                }
            } else {
                visited.insert(current_id.clone());
                for next_id in next_deps {
                    let mut new_chain = current_chain.clone();
                    new_chain.push(next_id.clone());
                    queue.push((next_id.clone(), new_chain));
                }
            }
        } else {
            // No further dependents, this is an end chain
            if current_chain.len() > 1 {
                chains.push(current_chain);
            }
        }
    }

    // Deduplicate chains (keep unique paths)
    chains.sort();
    chains.dedup();

    chains
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Estimate, Node, Task, WorkGraph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
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
        t2.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let index = build_reverse_index(&graph);
        assert_eq!(index.get("t1"), Some(&vec!["t2".to_string()]));
        assert!(!index.contains_key("t2"));
    }

    #[test]
    fn test_build_reverse_index_multiple_dependents() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let index = build_reverse_index(&graph);
        let dependents = index.get("t1").unwrap();
        assert_eq!(dependents.len(), 2);
        assert!(dependents.contains(&"t2".to_string()));
        assert!(dependents.contains(&"t3".to_string()));
    }

    #[test]
    fn test_collect_transitive_dependents() {
        let mut graph = WorkGraph::new();

        // t1 -> t2 -> t3 -> t4
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.after = vec!["t2".to_string()];
        let mut t4 = make_task("t4", "Task 4");
        t4.after = vec!["t3".to_string()];

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
        t2.after = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.after = vec!["t1".to_string()];
        let mut t4 = make_task("t4", "Task 4");
        t4.after = vec!["t2".to_string(), "t3".to_string()];

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
    fn test_impact_hours_calculation() {
        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Task 1");
        t1.estimate = Some(Estimate {
            hours: Some(8.0),
            cost: None,
        });

        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];
        t2.estimate = Some(Estimate {
            hours: Some(16.0),
            cost: None,
        });

        let mut t3 = make_task("t3", "Task 3");
        t3.after = vec!["t2".to_string()];
        t3.estimate = Some(Estimate {
            hours: Some(4.0),
            cost: None,
        });

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let index = build_reverse_index(&graph);
        let mut visited = HashSet::new();
        collect_transitive_dependents(&index, "t1", &mut visited);

        let mut total_hours = 0.0;
        for dep_id in &visited {
            if let Some(dep_task) = graph.get_task(dep_id)
                && let Some(ref estimate) = dep_task.estimate
            {
                total_hours += estimate.hours.unwrap_or(0.0);
            }
        }

        assert_eq!(total_hours, 20.0); // t2 (16) + t3 (4)
    }
}
