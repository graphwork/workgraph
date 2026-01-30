use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::graph::{Status, Task, WorkGraph};
use workgraph::parser::load_graph;

use super::graph_path;

/// A task in the trajectory with context flow info
#[derive(Debug, Serialize, Clone)]
pub struct TrajectoryStep {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub depth: usize,
    /// Artifacts/outputs this step receives from predecessors
    pub receives: Vec<String>,
    /// Artifacts/outputs this step produces for successors
    pub produces: Vec<String>,
    pub hours: Option<f64>,
}

/// A trajectory - a context-efficient path through tasks
#[derive(Debug, Serialize)]
pub struct Trajectory {
    pub root_task: String,
    pub steps: Vec<TrajectoryStep>,
    pub total_hours: Option<f64>,
    /// Tasks in order for claiming
    pub claim_order: Vec<String>,
}

/// Build reverse index: task_id -> tasks that depend on it (have it in blocked_by)
fn build_dependents_index(graph: &WorkGraph) -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();

    for task in graph.tasks() {
        for blocker in &task.blocked_by {
            index
                .entry(blocker.clone())
                .or_default()
                .push(task.id.clone());
        }
    }

    index
}

/// Find trajectory starting from a task
/// A trajectory follows the path where outputs become inputs
pub fn find_trajectory(graph: &WorkGraph, start_id: &str) -> Result<Trajectory> {
    let start_task = graph
        .get_task(start_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", start_id))?;

    let dependents_index = build_dependents_index(graph);
    let mut steps = Vec::new();
    let mut visited = HashSet::new();
    let mut claim_order = Vec::new();

    // BFS to find trajectory
    let mut queue: Vec<(&Task, usize, Vec<String>)> = vec![(start_task, 0, vec![])];

    while let Some((task, depth, receives)) = queue.pop() {
        if visited.contains(&task.id) {
            continue;
        }
        visited.insert(task.id.clone());

        // What this task produces (deliverables + artifacts)
        let mut produces: Vec<String> = task.deliverables.clone();
        for artifact in &task.artifacts {
            if !produces.contains(artifact) {
                produces.push(artifact.clone());
            }
        }

        let step = TrajectoryStep {
            id: task.id.clone(),
            title: task.title.clone(),
            status: task.status.clone(),
            depth,
            receives,
            produces: produces.clone(),
            hours: task.estimate.as_ref().and_then(|e| e.hours),
        };
        steps.push(step);
        claim_order.push(task.id.clone());

        // Find dependents that receive our outputs
        if let Some(dependent_ids) = dependents_index.get(&task.id) {
            for dep_id in dependent_ids {
                if visited.contains(dep_id) {
                    continue;
                }

                if let Some(dep_task) = graph.get_task(dep_id) {
                    // Check if dependent uses any of our outputs as inputs
                    let receives: Vec<String> = dep_task
                        .inputs
                        .iter()
                        .filter(|input| produces.contains(input))
                        .cloned()
                        .collect();

                    // Include in trajectory if:
                    // 1. Task receives context from us (has matching inputs), OR
                    // 2. Task is directly blocked by us (even without explicit inputs)
                    // This handles both explicit artifact flow and implicit dependency flow
                    queue.push((dep_task, depth + 1, receives));
                }
            }
        }
    }

    // Sort steps by depth for proper ordering
    steps.sort_by_key(|s| s.depth);

    let total_hours: Option<f64> = {
        let hours: Vec<f64> = steps.iter().filter_map(|s| s.hours).collect();
        if hours.is_empty() {
            None
        } else {
            Some(hours.iter().sum())
        }
    };

    Ok(Trajectory {
        root_task: start_id.to_string(),
        steps,
        total_hours,
        claim_order,
    })
}

/// Show trajectory for a task
pub fn run(dir: &Path, task_id: &str, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let trajectory = find_trajectory(&graph, task_id)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&trajectory)?);
    } else {
        println!("Trajectory from: {}", task_id);
        if let Some(hours) = trajectory.total_hours {
            println!("Total estimated: {}h", hours);
        }
        println!();

        for step in &trajectory.steps {
            let indent = "  ".repeat(step.depth);
            let status_str = match step.status {
                Status::Done => " [done]",
                Status::InProgress => " [in-progress]",
                Status::Failed => " [failed]",
                Status::Abandoned => " [abandoned]",
                _ => "",
            };
            let hours_str = step.hours.map(|h| format!(" ({}h)", h)).unwrap_or_default();

            println!("{}{} - {}{}{}", indent, step.id, step.title, hours_str, status_str);

            if !step.receives.is_empty() {
                println!("{}  ← receives: {}", indent, step.receives.join(", "));
            }
            if !step.produces.is_empty() && step.depth < trajectory.steps.iter().map(|s| s.depth).max().unwrap_or(0) {
                println!("{}  → produces: {}", indent, step.produces.join(", "));
            }
        }

        println!();
        println!("Claim order: {}", trajectory.claim_order.join(" → "));
    }

    Ok(())
}

/// Suggest optimal trajectory for an actor based on their capabilities
pub fn suggest_for_actor(dir: &Path, actor_id: &str, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let actor = graph
        .get_actor(actor_id)
        .ok_or_else(|| anyhow::anyhow!("Actor '{}' not found", actor_id))?;

    let actor_skills: HashSet<&String> = actor.capabilities.iter().collect();

    // Find ready tasks that match actor's skills
    let ready_tasks: Vec<&Task> = graph
        .tasks()
        .filter(|t| {
            t.status == Status::Open
                && t.blocked_by
                    .iter()
                    .all(|b| graph.get_task(b).map(|bt| bt.status == Status::Done).unwrap_or(true))
        })
        .collect();

    // Score trajectories starting from each ready task
    let mut trajectory_scores: Vec<(Trajectory, i32)> = Vec::new();

    for task in ready_tasks {
        let trajectory = find_trajectory(&graph, &task.id)?;

        // Score based on:
        // - Number of tasks actor can do (skill match)
        // - Context efficiency (artifacts flowing between tasks)
        // - Avoiding tasks already in progress or done
        let mut score = 0;
        let mut doable_count = 0;

        for step in &trajectory.steps {
            if step.status == Status::Done || step.status == Status::InProgress {
                continue; // Skip already handled
            }

            if let Some(t) = graph.get_task(&step.id) {
                let task_skills: HashSet<&String> = t.skills.iter().collect();
                let matched = actor_skills.intersection(&task_skills).count();
                let missing = task_skills.difference(&actor_skills).count();

                if missing == 0 || task_skills.is_empty() {
                    doable_count += 1;
                    score += 10;
                }
                score += (matched as i32) * 5;
                score -= (missing as i32) * 3;

                // Bonus for context flow
                if !step.receives.is_empty() {
                    score += 5;
                }
            }
        }

        // Only include trajectories where actor can do at least the first task
        if doable_count > 0 {
            trajectory_scores.push((trajectory, score));
        }
    }

    // Sort by score descending
    trajectory_scores.sort_by(|a, b| b.1.cmp(&a.1));

    if json {
        let output: Vec<_> = trajectory_scores
            .iter()
            .take(5)
            .map(|(t, score)| {
                serde_json::json!({
                    "root_task": t.root_task,
                    "score": score,
                    "steps": t.steps.len(),
                    "total_hours": t.total_hours,
                    "claim_order": t.claim_order,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Suggested trajectories for: {}", actor_id);
        println!();

        if trajectory_scores.is_empty() {
            println!("No suitable trajectories found.");
        } else {
            for (i, (trajectory, score)) in trajectory_scores.iter().take(5).enumerate() {
                let hours_str = trajectory
                    .total_hours
                    .map(|h| format!(" ({}h total)", h))
                    .unwrap_or_default();
                println!(
                    "{}. {} → {} tasks{} [score: {}]",
                    i + 1,
                    trajectory.root_task,
                    trajectory.steps.len(),
                    hours_str,
                    score
                );
                println!("   Path: {}", trajectory.claim_order.join(" → "));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Actor, ActorType, Node, TrustLevel};
    use workgraph::parser::save_graph;

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

    fn make_actor(id: &str, capabilities: Vec<&str>) -> Actor {
        Actor {
            id: id.to_string(),
            name: None,
            role: None,
            rate: None,
            capacity: None,
            capabilities: capabilities.into_iter().map(String::from).collect(),
            context_limit: None,
            trust_level: TrustLevel::Provisional,
            last_seen: None,
            actor_type: ActorType::Agent,
            matrix_user_id: None,
            response_times: vec![],
        }
    }

    #[test]
    fn test_simple_trajectory() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        // t1 produces output.txt, t2 needs it
        let mut t1 = make_task("t1", "Producer");
        t1.deliverables = vec!["output.txt".to_string()];

        let mut t2 = make_task("t2", "Consumer");
        t2.blocked_by = vec!["t1".to_string()];
        t2.inputs = vec!["output.txt".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        save_graph(&graph, &path).unwrap();

        let trajectory = find_trajectory(&graph, "t1").unwrap();
        assert_eq!(trajectory.steps.len(), 2);
        assert_eq!(trajectory.claim_order, vec!["t1", "t2"]);
    }

    #[test]
    fn test_trajectory_with_no_dependents() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Standalone");
        graph.add_node(Node::Task(t1));
        save_graph(&graph, &path).unwrap();

        let trajectory = find_trajectory(&graph, "t1").unwrap();
        assert_eq!(trajectory.steps.len(), 1);
    }

    #[test]
    fn test_trajectory_chain() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Step 1");
        t1.deliverables = vec!["a.txt".to_string()];

        let mut t2 = make_task("t2", "Step 2");
        t2.blocked_by = vec!["t1".to_string()];
        t2.inputs = vec!["a.txt".to_string()];
        t2.deliverables = vec!["b.txt".to_string()];

        let mut t3 = make_task("t3", "Step 3");
        t3.blocked_by = vec!["t2".to_string()];
        t3.inputs = vec!["b.txt".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        save_graph(&graph, &path).unwrap();

        let trajectory = find_trajectory(&graph, "t1").unwrap();
        assert_eq!(trajectory.steps.len(), 3);
        assert_eq!(trajectory.claim_order, vec!["t1", "t2", "t3"]);
    }

    #[test]
    fn test_suggest_for_actor() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Rust Task");
        t1.skills = vec!["rust".to_string()];

        let actor = make_actor("rust-dev", vec!["rust"]);

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Actor(actor));
        save_graph(&graph, &path).unwrap();

        let result = suggest_for_actor(temp_dir.path(), "rust-dev", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_trajectory() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Test");
        graph.add_node(Node::Task(t1));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }
}
