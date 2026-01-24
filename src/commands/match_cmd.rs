use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use workgraph::graph::TrustLevel;
use workgraph::parser::load_graph;

use super::graph_path;

/// Match result for an actor
#[derive(Debug, Serialize)]
struct MatchResult {
    actor_id: String,
    actor_name: Option<String>,
    score: u32,
    matched_skills: Vec<String>,
    missing_skills: Vec<String>,
    trust_level: TrustLevel,
    available: bool,
}

/// Find actors capable of performing a task
pub fn run(dir: &Path, task_id: &str, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    let required_skills: std::collections::HashSet<_> = task.skills.iter().collect();

    let mut matches: Vec<MatchResult> = graph
        .actors()
        .map(|actor| {
            let actor_skills: std::collections::HashSet<_> = actor.capabilities.iter().collect();

            let matched: Vec<_> = required_skills
                .intersection(&actor_skills)
                .map(|s| (*s).clone())
                .collect();

            let missing: Vec<_> = required_skills
                .difference(&actor_skills)
                .map(|s| (*s).clone())
                .collect();

            // Score: matched skills count, bonus for verified trust
            let mut score = matched.len() as u32;
            if actor.trust_level == TrustLevel::Verified {
                score += 1;
            }

            // Check if actor is currently working on something
            let available = !graph.tasks().any(|t| {
                t.assigned.as_ref() == Some(&actor.id)
                    && t.status == workgraph::graph::Status::InProgress
            });

            MatchResult {
                actor_id: actor.id.clone(),
                actor_name: actor.name.clone(),
                score,
                matched_skills: matched,
                missing_skills: missing,
                trust_level: actor.trust_level.clone(),
                available,
            }
        })
        .collect();

    // Sort by score descending, then by availability
    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.available.cmp(&a.available))
    });

    // Filter to only include actors with at least partial match (or all if no skills required)
    let matches: Vec<_> = if required_skills.is_empty() {
        matches
    } else {
        matches.into_iter().filter(|m| m.score > 0).collect()
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&matches)?);
    } else {
        println!("Task: {} - {}", task.id, task.title);
        if task.skills.is_empty() {
            println!("Required skills: (none)");
        } else {
            println!("Required skills: {}", task.skills.join(", "));
        }
        println!();

        if matches.is_empty() {
            if required_skills.is_empty() {
                println!("No actors registered.");
            } else {
                println!("No actors with matching capabilities found.");
            }
        } else {
            println!("Capable actors:");
            for m in &matches {
                let name = m.actor_name.as_deref().unwrap_or(&m.actor_id);
                let available_str = if m.available { "" } else { " [BUSY]" };
                let trust_str = match m.trust_level {
                    TrustLevel::Verified => " [verified]",
                    TrustLevel::Unknown => " [unknown]",
                    TrustLevel::Provisional => "",
                };

                if required_skills.is_empty() {
                    println!("  {} - {}{}{}", m.actor_id, name, trust_str, available_str);
                } else if m.missing_skills.is_empty() {
                    println!(
                        "  {} - {} (all skills matched){}{} ",
                        m.actor_id, name, trust_str, available_str
                    );
                } else {
                    println!(
                        "  {} - {} (missing: {}){}{} ",
                        m.actor_id,
                        name,
                        m.missing_skills.join(", "),
                        trust_str,
                        available_str
                    );
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Actor, Node, Status, Task, WorkGraph};
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
        }
    }

    #[test]
    fn test_match_with_skills() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Rust task");
        task.skills = vec!["rust".to_string(), "testing".to_string()];

        let actor1 = make_actor("rust-expert", vec!["rust", "testing", "documentation"]);
        let actor2 = make_actor("python-dev", vec!["python", "testing"]);

        graph.add_node(Node::Task(task));
        graph.add_node(Node::Actor(actor1));
        graph.add_node(Node::Actor(actor2));

        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }
}
