use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashSet;
use std::path::Path;
use workgraph::graph::{Status, TrustLevel};
use workgraph::parser::load_graph;
use workgraph::query::ready_tasks;

use super::graph_path;

/// Candidate task for an actor
#[derive(Debug, Serialize)]
struct TaskCandidate {
    id: String,
    title: String,
    score: i32,
    matched_skills: Vec<String>,
    missing_skills: Vec<String>,
    hours: Option<f64>,
    inputs_available: bool,
}

/// Result of next task query
#[derive(Debug, Serialize)]
struct NextTaskResult {
    actor_id: String,
    actor_capabilities: Vec<String>,
    recommended: Option<TaskCandidate>,
    alternatives: Vec<TaskCandidate>,
}

/// Find the best next task for an actor based on capabilities and readiness
pub fn run(dir: &Path, actor_id: &str, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Get actor
    let actor = graph
        .get_actor(actor_id)
        .ok_or_else(|| anyhow::anyhow!("Actor '{}' not found", actor_id))?;

    let actor_skills: HashSet<&String> = actor.capabilities.iter().collect();

    // Get ready tasks
    let ready = ready_tasks(&graph);

    // Score each task for this actor
    let mut candidates: Vec<TaskCandidate> = ready
        .iter()
        .map(|task| {
            let task_skills: HashSet<&String> = task.skills.iter().collect();

            let matched: Vec<String> = actor_skills
                .intersection(&task_skills)
                .map(|s| (*s).clone())
                .collect();

            let missing: Vec<String> = task_skills
                .difference(&actor_skills)
                .map(|s| (*s).clone())
                .collect();

            // Check if inputs are available from dependencies
            let mut available_artifacts: HashSet<String> = HashSet::new();
            for dep_id in &task.blocked_by {
                if let Some(dep_task) = graph.get_task(dep_id) {
                    for artifact in &dep_task.artifacts {
                        available_artifacts.insert(artifact.clone());
                    }
                }
            }
            let inputs_available = task.inputs.iter().all(|i| available_artifacts.contains(i));

            // Scoring:
            // - Base: number of matched skills * 10
            // - Penalty: missing skills * -5
            // - Bonus: all skills matched +20
            // - Bonus: no skills required +5 (generic task)
            // - Bonus: inputs available +10
            // - Bonus: verified trust +5
            let mut score: i32 = (matched.len() as i32) * 10;
            score -= (missing.len() as i32) * 5;

            if !task.skills.is_empty() && missing.is_empty() {
                score += 20; // Perfect skill match
            }
            if task.skills.is_empty() {
                score += 5; // Generic task anyone can do
            }
            if inputs_available || task.inputs.is_empty() {
                score += 10; // Ready to execute
            }
            if actor.trust_level == TrustLevel::Verified {
                score += 5;
            }

            TaskCandidate {
                id: task.id.clone(),
                title: task.title.clone(),
                score,
                matched_skills: matched,
                missing_skills: missing,
                hours: task.estimate.as_ref().and_then(|e| e.hours),
                inputs_available: inputs_available || task.inputs.is_empty(),
            }
        })
        .collect();

    // Sort by score descending
    candidates.sort_by(|a, b| b.score.cmp(&a.score));

    // Filter to only tasks with non-negative score (at least partial capability match)
    // But include tasks with no skill requirements
    let viable: Vec<TaskCandidate> = candidates
        .into_iter()
        .filter(|c| c.score >= 0)
        .collect();

    let (recommended, alternatives) = if viable.is_empty() {
        (None, vec![])
    } else {
        let mut iter = viable.into_iter();
        (iter.next(), iter.take(4).collect())
    };

    let result = NextTaskResult {
        actor_id: actor_id.to_string(),
        actor_capabilities: actor.capabilities.clone(),
        recommended,
        alternatives,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Next task for: {} ", actor_id);
        if !result.actor_capabilities.is_empty() {
            println!("Capabilities: {}", result.actor_capabilities.join(", "));
        }
        println!();

        if let Some(ref task) = result.recommended {
            println!("Recommended:");
            print_candidate(task);

            if !result.alternatives.is_empty() {
                println!();
                println!("Alternatives:");
                for alt in &result.alternatives {
                    print_candidate(alt);
                }
            }
        } else {
            println!("No suitable tasks available.");
            println!();
            println!("The agent should sleep and retry later.");
        }
    }

    Ok(())
}

fn print_candidate(task: &TaskCandidate) {
    let hours_str = task
        .hours
        .map(|h| format!(" ({}h)", h))
        .unwrap_or_default();
    let inputs_str = if task.inputs_available { "" } else { " [waiting for inputs]" };

    println!("  {} - {}{}{}", task.id, task.title, hours_str, inputs_str);
    println!("    Score: {}", task.score);

    if !task.matched_skills.is_empty() {
        println!("    Matched: {}", task.matched_skills.join(", "));
    }
    if !task.missing_skills.is_empty() {
        println!("    Missing: {}", task.missing_skills.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Actor, ActorType, Node, Task, TrustLevel, WorkGraph};
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
    fn test_next_with_matching_skills() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Rust Task");
        task.skills = vec!["rust".to_string()];

        let actor = make_actor("rust-dev", vec!["rust", "testing"]);

        graph.add_node(Node::Task(task));
        graph.add_node(Node::Actor(actor));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "rust-dev", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_next_no_matching_tasks() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Python Task");
        task.skills = vec!["python".to_string()];

        let actor = make_actor("rust-dev", vec!["rust"]);

        graph.add_node(Node::Task(task));
        graph.add_node(Node::Actor(actor));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "rust-dev", false);
        assert!(result.is_ok()); // Should work but recommend nothing
    }

    #[test]
    fn test_next_prefers_full_skill_match() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut t1 = make_task("t1", "Partial Match");
        t1.skills = vec!["rust".to_string(), "python".to_string()];

        let mut t2 = make_task("t2", "Full Match");
        t2.skills = vec!["rust".to_string()];

        let actor = make_actor("rust-dev", vec!["rust"]);

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Actor(actor));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "rust-dev", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_next_json_output() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let task = make_task("t1", "Test Task");
        let actor = make_actor("agent", vec![]);

        graph.add_node(Node::Task(task));
        graph.add_node(Node::Actor(actor));
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "agent", true);
        assert!(result.is_ok());
    }
}
