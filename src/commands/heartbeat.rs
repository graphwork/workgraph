use anyhow::Result;
use chrono::Utc;
use std::path::Path;
use workgraph::service::AgentRegistry;

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

/// Record heartbeat for an agent
///
/// Validates the ID is an agent ID (agent-N format) before recording.
pub fn run_auto(dir: &Path, id: &str) -> Result<()> {
    if is_agent_id(id) {
        run_agent(dir, id)
    } else {
        anyhow::bail!(
            "Unknown ID '{}'. Actor nodes have been removed. Use agent IDs (e.g., agent-1).",
            id
        )
    }
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
        println!(
            "Agent heartbeat status (threshold: {} minutes):",
            threshold_minutes
        );
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
    use workgraph::graph::WorkGraph;
    use workgraph::parser::save_graph;

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
    fn test_heartbeat_non_agent_fails() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // Actor nodes no longer exist, so heartbeat for non-agent IDs should fail
        let result = run_auto(temp_dir.path(), "test-agent");
        assert!(result.is_err());
    }

    #[test]
    fn test_check_agents_no_agents() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // Should succeed with no agents registered
        let result = run_check_agents(temp_dir.path(), 5, false);
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
        let original_hb = registry
            .get_agent("agent-1")
            .unwrap()
            .last_heartbeat
            .clone();

        // Wait a tiny bit
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Record heartbeat
        let result = run_agent(temp_dir.path(), "agent-1");
        assert!(result.is_ok());

        // Verify heartbeat was updated
        let registry = AgentRegistry::load(temp_dir.path()).unwrap();
        let new_hb = registry
            .get_agent("agent-1")
            .unwrap()
            .last_heartbeat
            .clone();
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
    fn test_run_auto_with_non_agent_fails() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // Non-agent IDs now fail since Actor nodes are removed
        let result = run_auto(temp_dir.path(), "test-agent");
        assert!(result.is_err());
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
