use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;
use workgraph::agency;
use workgraph::graph::TrustLevel;
use workgraph::parser::load_graph;

use super::graph_path;

/// Match result for an agent
#[derive(Debug, Serialize)]
struct MatchResult {
    agent_id: String,
    agent_name: String,
    score: u32,
    matched_skills: Vec<String>,
    missing_skills: Vec<String>,
    trust_level: TrustLevel,
    available: bool,
}

/// Find agents capable of performing a task
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

    // Load agents from .workgraph/agency/agents/
    let agents_dir = dir.join("agency").join("agents");
    let agents = agency::load_all_agents(&agents_dir).context("Failed to load agents")?;

    let mut matches: Vec<MatchResult> = agents
        .iter()
        .map(|agent| {
            let agent_skills: std::collections::HashSet<_> = agent.capabilities.iter().collect();

            let matched: Vec<_> = required_skills
                .intersection(&agent_skills)
                .map(|s| (*s).clone())
                .collect();

            let missing: Vec<_> = required_skills
                .difference(&agent_skills)
                .map(|s| (*s).clone())
                .collect();

            // Score: matched skills count, bonus for verified trust
            let mut score = matched.len() as u32;
            if agent.trust_level == TrustLevel::Verified {
                score += 1;
            }

            // Check if agent is currently working on something
            let available = !graph.tasks().any(|t| {
                t.agent.as_ref() == Some(&agent.id)
                    && t.status == workgraph::graph::Status::InProgress
            });

            MatchResult {
                agent_id: agent.id.clone(),
                agent_name: agent.name.clone(),
                score,
                matched_skills: matched,
                missing_skills: missing,
                trust_level: agent.trust_level.clone(),
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

    // Filter to only include agents with at least partial match (or all if no skills required)
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
                println!("No agents registered.");
            } else {
                println!("No agents with matching capabilities found.");
            }
        } else {
            println!("Capable agents:");
            for m in &matches {
                let available_str = if m.available { "" } else { " [BUSY]" };
                let trust_str = match m.trust_level {
                    TrustLevel::Verified => " [verified]",
                    TrustLevel::Unknown => " [unknown]",
                    TrustLevel::Provisional => "",
                };
                let short_id = agency::short_hash(&m.agent_id);

                if required_skills.is_empty() {
                    println!(
                        "  {} - {}{}{}",
                        short_id, m.agent_name, trust_str, available_str
                    );
                } else if m.missing_skills.is_empty() {
                    println!(
                        "  {} - {} (all skills matched){}{} ",
                        short_id, m.agent_name, trust_str, available_str
                    );
                } else {
                    println!(
                        "  {} - {} (missing: {}){}{} ",
                        short_id,
                        m.agent_name,
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
    use workgraph::agency::{Agent, Lineage, PerformanceRecord};
    use workgraph::graph::{Node, Status, Task, TrustLevel, WorkGraph};
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
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
        }
    }

    fn make_agent(name: &str, capabilities: Vec<&str>) -> Agent {
        let role_id = format!("{}-role", name);
        let mot_id = format!("{}-mot", name);
        let id = agency::content_hash_agent(&role_id, &mot_id);
        Agent {
            id,
            role_id,
            motivation_id: mot_id,
            name: name.to_string(),
            performance: PerformanceRecord {
                task_count: 0,
                avg_score: None,
                evaluations: vec![],
            },
            lineage: Lineage::default(),
            capabilities: capabilities.into_iter().map(String::from).collect(),
            rate: None,
            capacity: None,
            trust_level: TrustLevel::Provisional,
            contact: None,
            executor: "claude".to_string(),
        }
    }

    fn setup_agents(dir: &Path, agents: &[Agent]) {
        let agency_dir = dir.join("agency");
        agency::init(&agency_dir).unwrap();
        let agents_dir = agency_dir.join("agents");
        for agent in agents {
            agency::save_agent(agent, &agents_dir).unwrap();
        }
    }

    #[test]
    fn test_match_with_skills() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Rust task");
        task.skills = vec!["rust".to_string(), "testing".to_string()];

        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        let agent1 = make_agent("rust-expert", vec!["rust", "testing", "documentation"]);
        let agent2 = make_agent("python-dev", vec!["python", "testing"]);
        setup_agents(temp_dir.path(), &[agent1, agent2]);

        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_match_nonexistent_task() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "no-such-task", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_match_task_with_no_skills() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Generic task")));
        save_graph(&graph, &path).unwrap();

        let agent = make_agent("generalist", vec!["rust"]);
        setup_agents(temp_dir.path(), &[agent]);

        // No skills required — all agents should match
        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_match_no_agents_registered() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Orphan task");
        task.skills = vec!["rust".to_string()];
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        // Initialize agency but add no agents
        let agency_dir = temp_dir.path().join("agency");
        agency::init(&agency_dir).unwrap();

        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_match_json_output() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Rust task");
        task.skills = vec!["rust".to_string()];
        graph.add_node(Node::Task(task));
        save_graph(&graph, &path).unwrap();

        let agent = make_agent("rust-dev", vec!["rust"]);
        setup_agents(temp_dir.path(), &[agent]);

        let result = run(temp_dir.path(), "t1", true);
        assert!(result.is_ok());
    }
}
