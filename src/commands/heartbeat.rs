use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::parser::{load_graph, save_graph};
use workgraph::service::AgentRegistry;

use super::graph_path;

/// Update an actor's last_seen timestamp (heartbeat)
///
/// This is for actors defined in the graph (humans, teams, etc.)
pub fn run(dir: &Path, actor_id: &str) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let actor = graph
        .get_actor_mut(actor_id)
        .ok_or_else(|| anyhow::anyhow!("Actor '{}' not found", actor_id))?;

    let now = Utc::now().to_rfc3339();
    actor.last_seen = Some(now.clone());

    save_graph(&graph, &path).context("Failed to save graph")?;

    println!("Heartbeat recorded for '{}' at {}", actor_id, now);
    Ok(())
}

/// Update an agent's last_heartbeat timestamp
///
/// This is for agent processes registered in the service registry.
/// Agent IDs are in the format "agent-N" (e.g., agent-1, agent-7).
pub fn run_agent(dir: &Path, agent_id: &str) -> Result<()> {
    let mut registry = AgentRegistry::load_locked(dir)?;

    let now = Utc::now().to_rfc3339();
    registry.update_heartbeat(agent_id)?;
    registry.save()?;

    println!("Agent heartbeat recorded for '{}' at {}", agent_id, now);
    Ok(())
}

/// Check if the given ID is an agent ID (starts with "agent-")
pub fn is_agent_id(id: &str) -> bool {
    id.starts_with("agent-")
}

/// Record heartbeat for either an actor or an agent
///
/// Automatically detects whether the ID refers to an agent (agent-N format)
/// or an actor defined in the graph.
pub fn run_auto(dir: &Path, id: &str) -> Result<()> {
    if is_agent_id(id) {
        run_agent(dir, id)
    } else {
        run(dir, id)
    }
}

/// Check for stale actors (no heartbeat within threshold)
pub fn run_check(dir: &Path, threshold_minutes: u64, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let now = Utc::now();
    let threshold = chrono::Duration::minutes(threshold_minutes as i64);

    let mut stale_actors = Vec::new();
    let mut active_actors = Vec::new();

    for actor in graph.actors() {
        if let Some(ref last_seen_str) = actor.last_seen {
            if let Ok(last_seen) = chrono::DateTime::parse_from_rfc3339(last_seen_str) {
                let elapsed = now.signed_duration_since(last_seen);
                if elapsed > threshold {
                    stale_actors.push((actor.id.clone(), last_seen_str.clone(), elapsed.num_minutes()));
                } else {
                    active_actors.push((actor.id.clone(), last_seen_str.clone(), elapsed.num_minutes()));
                }
            }
        } else {
            // Never seen - considered stale
            stale_actors.push((actor.id.clone(), "never".to_string(), -1));
        }
    }

    if json {
        let output = serde_json::json!({
            "threshold_minutes": threshold_minutes,
            "stale": stale_actors.iter().map(|(id, last_seen, mins)| {
                serde_json::json!({
                    "id": id,
                    "last_seen": last_seen,
                    "minutes_ago": mins,
                })
            }).collect::<Vec<_>>(),
            "active": active_actors.iter().map(|(id, last_seen, mins)| {
                serde_json::json!({
                    "id": id,
                    "last_seen": last_seen,
                    "minutes_ago": mins,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Heartbeat status (threshold: {} minutes):", threshold_minutes);
        println!();

        if !active_actors.is_empty() {
            println!("Active actors:");
            for (id, _, mins) in &active_actors {
                println!("  {} (seen {} min ago)", id, mins);
            }
        }

        if !stale_actors.is_empty() {
            println!();
            println!("Stale actors (may be dead):");
            for (id, last_seen, mins) in &stale_actors {
                if *mins < 0 {
                    println!("  {} (never seen)", id);
                } else {
                    println!("  {} (last seen {} min ago: {})", id, mins, last_seen);
                }
            }
        }

        if active_actors.is_empty() && stale_actors.is_empty() {
            println!("No actors registered.");
        }
    }

    Ok(())
}

/// Check for stale agents (no heartbeat within threshold)
///
/// This checks agent processes registered in the service registry.
pub fn run_check_agents(dir: &Path, threshold_minutes: u64, json: bool) -> Result<()> {
    let registry = AgentRegistry::load(dir)?;
    let threshold_secs = (threshold_minutes * 60) as i64;

    let mut stale_agents = Vec::new();
    let mut active_agents = Vec::new();
    let mut dead_agents = Vec::new();

    for agent in registry.list_agents() {
        // Already marked as dead
        if agent.status == workgraph::service::AgentStatus::Dead {
            dead_agents.push((
                agent.id.clone(),
                agent.task_id.clone(),
                agent.last_heartbeat.clone(),
            ));
            continue;
        }

        // Not alive (done, failed, stopping)
        if !agent.is_alive() {
            continue;
        }

        if let Some(secs) = agent.seconds_since_heartbeat() {
            let mins = secs / 60;
            if secs > threshold_secs {
                stale_agents.push((
                    agent.id.clone(),
                    agent.task_id.clone(),
                    agent.last_heartbeat.clone(),
                    mins,
                ));
            } else {
                active_agents.push((
                    agent.id.clone(),
                    agent.task_id.clone(),
                    agent.last_heartbeat.clone(),
                    mins,
                ));
            }
        } else {
            // Can't parse heartbeat - consider stale
            stale_agents.push((
                agent.id.clone(),
                agent.task_id.clone(),
                agent.last_heartbeat.clone(),
                -1,
            ));
        }
    }

    if json {
        let output = serde_json::json!({
            "threshold_minutes": threshold_minutes,
            "stale": stale_agents.iter().map(|(id, task, last_hb, mins)| {
                serde_json::json!({
                    "id": id,
                    "task_id": task,
                    "last_heartbeat": last_hb,
                    "minutes_ago": mins,
                })
            }).collect::<Vec<_>>(),
            "active": active_agents.iter().map(|(id, task, last_hb, mins)| {
                serde_json::json!({
                    "id": id,
                    "task_id": task,
                    "last_heartbeat": last_hb,
                    "minutes_ago": mins,
                })
            }).collect::<Vec<_>>(),
            "dead": dead_agents.iter().map(|(id, task, last_hb)| {
                serde_json::json!({
                    "id": id,
                    "task_id": task,
                    "last_heartbeat": last_hb,
                })
            }).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Agent heartbeat status (threshold: {} minutes):", threshold_minutes);
        println!();

        if !active_agents.is_empty() {
            println!("Active agents:");
            for (id, task, _, mins) in &active_agents {
                println!("  {} on '{}' (heartbeat {} min ago)", id, task, mins);
            }
        }

        if !stale_agents.is_empty() {
            println!();
            println!("Stale agents (may be dead):");
            for (id, task, last_hb, mins) in &stale_agents {
                if *mins < 0 {
                    println!("  {} on '{}' (invalid heartbeat: {})", id, task, last_hb);
                } else {
                    println!("  {} on '{}' (last heartbeat {} min ago)", id, task, mins);
                }
            }
        }

        if !dead_agents.is_empty() {
            println!();
            println!("Dead agents:");
            for (id, task, _) in &dead_agents {
                println!("  {} was on '{}'", id, task);
            }
        }

        if active_agents.is_empty() && stale_agents.is_empty() && dead_agents.is_empty() {
            println!("No agents registered.");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::{Actor, ActorType, Node, TrustLevel, WorkGraph};
    use workgraph::parser::save_graph;

    fn setup_with_actor() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        let actor = Actor {
            id: "test-agent".to_string(),
            name: Some("Test Agent".to_string()),
            role: Some("agent".to_string()),
            rate: None,
            capacity: None,
            capabilities: vec!["rust".to_string()],
            context_limit: Some(100000),
            trust_level: TrustLevel::Provisional,
            last_seen: None,
            actor_type: ActorType::Agent,
            matrix_user_id: None,
            response_times: vec![],
        };
        graph.add_node(Node::Actor(actor));
        save_graph(&graph, &path).unwrap();

        temp_dir
    }

    fn setup_with_agent() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        // Create a graph file first
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // Register an agent
        let mut registry = AgentRegistry::new();
        registry.register_agent(12345, "test-task", "claude", "/tmp/output.log");
        registry.save(temp_dir.path()).unwrap();

        temp_dir
    }

    #[test]
    fn test_heartbeat() {
        let temp_dir = setup_with_actor();

        let result = run(temp_dir.path(), "test-agent");
        assert!(result.is_ok());

        // Verify last_seen was updated
        let graph = load_graph(&graph_path(temp_dir.path())).unwrap();
        let actor = graph.get_actor("test-agent").unwrap();
        assert!(actor.last_seen.is_some());
    }

    #[test]
    fn test_heartbeat_unknown_actor() {
        let temp_dir = setup_with_actor();

        let result = run(temp_dir.path(), "unknown-agent");
        assert!(result.is_err());
    }

    #[test]
    fn test_check_stale() {
        let temp_dir = setup_with_actor();

        // Actor has no heartbeat yet, should be stale
        let result = run_check(temp_dir.path(), 5, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_is_agent_id() {
        assert!(is_agent_id("agent-1"));
        assert!(is_agent_id("agent-42"));
        assert!(is_agent_id("agent-999"));
        assert!(!is_agent_id("erik"));
        assert!(!is_agent_id("test-agent"));
        assert!(!is_agent_id("claude-agent"));
    }

    #[test]
    fn test_agent_heartbeat() {
        let temp_dir = setup_with_agent();

        // Get initial heartbeat
        let registry = AgentRegistry::load(temp_dir.path()).unwrap();
        let original_hb = registry.get_agent("agent-1").unwrap().last_heartbeat.clone();

        // Wait a tiny bit
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Record heartbeat
        let result = run_agent(temp_dir.path(), "agent-1");
        assert!(result.is_ok());

        // Verify heartbeat was updated
        let registry = AgentRegistry::load(temp_dir.path()).unwrap();
        let new_hb = registry.get_agent("agent-1").unwrap().last_heartbeat.clone();
        assert_ne!(original_hb, new_hb);
    }

    #[test]
    fn test_agent_heartbeat_unknown() {
        let temp_dir = setup_with_agent();

        let result = run_agent(temp_dir.path(), "agent-999");
        assert!(result.is_err());
    }

    #[test]
    fn test_run_auto_with_agent() {
        let temp_dir = setup_with_agent();

        // Should detect agent-1 as an agent ID and use run_agent
        let result = run_auto(temp_dir.path(), "agent-1");
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_auto_with_actor() {
        let temp_dir = setup_with_actor();

        // Should detect test-agent as an actor ID and use run
        let result = run_auto(temp_dir.path(), "test-agent");
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_agents_empty() {
        let temp_dir = TempDir::new().unwrap();
        // Create graph file
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // No agents registered
        let result = run_check_agents(temp_dir.path(), 5, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_agents_with_active() {
        let temp_dir = setup_with_agent();

        // Agent was just registered, should be active
        let result = run_check_agents(temp_dir.path(), 5, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_agents_json() {
        let temp_dir = setup_with_agent();

        // Should output valid JSON
        let result = run_check_agents(temp_dir.path(), 5, true);
        assert!(result.is_ok());
    }
}
