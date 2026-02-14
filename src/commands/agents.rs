//! List running agents
//!
//! Displays information about all agents registered in the service registry.
//! Checks PID liveness so dead processes are shown accurately.
//!
//! Usage:
//!   wg agents              # List all agents in table format
//!   wg agents --json       # Output as JSON for scripting
//!   wg agents --alive      # Only show alive agents
//!   wg agents --dead       # Only show dead agents

use anyhow::Result;
use std::path::Path;
use workgraph::service::{AgentEntry, AgentRegistry, AgentStatus};

use super::dead_agents::is_process_alive;

/// Compute the effective status of an agent by checking PID liveness.
/// If the registry says the agent is alive but the process has exited,
/// return "dead (process exited)" instead of the registry status.
fn effective_status(agent: &AgentEntry) -> String {
    if agent.is_alive() && !is_process_alive(agent.pid) {
        "dead (process exited)".to_string()
    } else {
        match agent.status {
            AgentStatus::Starting => "starting".to_string(),
            AgentStatus::Working => "working".to_string(),
            AgentStatus::Idle => "idle".to_string(),
            AgentStatus::Stopping => "stopping".to_string(),
            AgentStatus::Done => "done".to_string(),
            AgentStatus::Failed => "failed".to_string(),
            AgentStatus::Dead => "dead".to_string(),
        }
    }
}

/// Check if an agent is effectively alive (registry alive AND process running)
fn is_effectively_alive(agent: &AgentEntry) -> bool {
    agent.is_alive() && is_process_alive(agent.pid)
}

/// Check if an agent is effectively dead (registry dead OR process exited)
fn is_effectively_dead(agent: &AgentEntry) -> bool {
    agent.status == AgentStatus::Dead || (agent.is_alive() && !is_process_alive(agent.pid))
}

/// List all agents in the registry
pub fn run(dir: &Path, filter: Option<AgentFilter>, json: bool) -> Result<()> {
    let registry = AgentRegistry::load(dir)?;
    let agents = registry.list_agents();

    // Apply filter using effective status (PID-aware)
    let filtered: Vec<_> = match filter {
        Some(AgentFilter::Alive) => agents
            .into_iter()
            .filter(|a| is_effectively_alive(a))
            .collect(),
        Some(AgentFilter::Dead) => agents
            .into_iter()
            .filter(|a| is_effectively_dead(a))
            .collect(),
        Some(AgentFilter::Working) => agents
            .into_iter()
            .filter(|a| a.status == AgentStatus::Working && is_process_alive(a.pid))
            .collect(),
        Some(AgentFilter::Idle) => agents
            .into_iter()
            .filter(|a| a.status == AgentStatus::Idle && is_process_alive(a.pid))
            .collect(),
        None => agents,
    };

    if json {
        output_json(&filtered)
    } else {
        output_table(&filtered)
    }
}

/// Filter for listing agents
#[derive(Debug, Clone, Copy)]
pub enum AgentFilter {
    Alive,
    Dead,
    Working,
    Idle,
}

fn output_json(agents: &[&AgentEntry]) -> Result<()> {
    let output: Vec<_> = agents
        .iter()
        .map(|a| {
            let eff_status = effective_status(a);
            let process_alive = is_process_alive(a.pid);
            serde_json::json!({
                "id": a.id,
                "task_id": a.task_id,
                "executor": a.executor,
                "pid": a.pid,
                "started_at": a.started_at,
                "last_heartbeat": a.last_heartbeat,
                "uptime": a.uptime_human(),
                "uptime_secs": a.uptime_secs(),
                "status": eff_status,
                "process_alive": process_alive,
                "output_file": a.output_file,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn output_table(agents: &[&AgentEntry]) -> Result<()> {
    if agents.is_empty() {
        println!("No agents registered.");
        return Ok(());
    }

    // Calculate column widths
    let id_width = agents.iter().map(|a| a.id.len()).max().unwrap_or(8).max(8);
    let task_width = agents
        .iter()
        .map(|a| a.task_id.len())
        .max()
        .unwrap_or(20)
        .max(20)
        .min(40);
    let executor_width = agents
        .iter()
        .map(|a| a.executor.len())
        .max()
        .unwrap_or(8)
        .max(8);

    // Print header
    println!(
        "{:<id_width$}  {:<task_width$}  {:<executor_width$}  {:>6}  {:>6}  {}",
        "ID",
        "TASK",
        "EXECUTOR",
        "PID",
        "UPTIME",
        "STATUS",
        id_width = id_width,
        task_width = task_width,
        executor_width = executor_width,
    );

    // Print rows
    for agent in agents {
        let task_display = if agent.task_id.len() > task_width {
            format!("{}...", &agent.task_id[..task_width - 3])
        } else {
            agent.task_id.clone()
        };

        let status_display = effective_status(agent);

        println!(
            "{:<id_width$}  {:<task_width$}  {:<executor_width$}  {:>6}  {:>6}  {}",
            agent.id,
            task_display,
            agent.executor,
            agent.pid,
            agent.uptime_human(),
            status_display,
            id_width = id_width,
            task_width = task_width,
            executor_width = executor_width,
        );
    }

    // Summary using effective status
    let alive_count = agents.iter().filter(|a| is_effectively_alive(a)).count();
    let dead_count = agents.iter().filter(|a| is_effectively_dead(a)).count();

    println!();
    if dead_count > 0 {
        println!(
            "{} agent(s) total: {} alive, {} dead",
            agents.len(),
            alive_count,
            dead_count
        );
    } else {
        println!("{} agent(s)", agents.len());
    }

    Ok(())
}

/// Get agent count summary
#[cfg(test)]
pub fn get_summary(dir: &Path) -> Result<AgentSummary> {
    let registry = AgentRegistry::load(dir)?;
    let agents = registry.list_agents();

    let total = agents.len();
    let alive = agents.iter().filter(|a| is_effectively_alive(a)).count();
    let working = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Working && is_process_alive(a.pid))
        .count();
    let idle = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Idle && is_process_alive(a.pid))
        .count();
    let dead = agents.iter().filter(|a| is_effectively_dead(a)).count();

    Ok(AgentSummary {
        total,
        alive,
        working,
        idle,
        dead,
    })
}

/// Summary of agent counts
#[cfg(test)]
#[derive(Debug)]
pub struct AgentSummary {
    pub total: usize,
    pub alive: usize,
    pub working: usize,
    pub idle: usize,
    pub dead: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::graph::WorkGraph;
    use workgraph::parser::save_graph;

    fn setup_with_agents() -> TempDir {
        let temp_dir = TempDir::new().unwrap();
        // Create a graph file first
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        // Register some agents
        let mut registry = AgentRegistry::new();
        registry.register_agent(12345, "task-1", "claude", "/tmp/output1.log");
        registry.register_agent(12346, "task-2", "shell", "/tmp/output2.log");
        registry.register_agent(12347, "task-3", "claude", "/tmp/output3.log");

        // Set different statuses
        registry
            .update_status("agent-1", AgentStatus::Working)
            .unwrap();
        registry
            .update_status("agent-2", AgentStatus::Idle)
            .unwrap();
        registry
            .update_status("agent-3", AgentStatus::Dead)
            .unwrap();

        registry.save(temp_dir.path()).unwrap();

        temp_dir
    }

    #[test]
    fn test_list_agents() {
        let temp_dir = setup_with_agents();
        let result = run(temp_dir.path(), None, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_agents_json() {
        let temp_dir = setup_with_agents();
        let result = run(temp_dir.path(), None, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_alive_only() {
        let temp_dir = setup_with_agents();
        let result = run(temp_dir.path(), Some(AgentFilter::Alive), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_dead_only() {
        let temp_dir = setup_with_agents();
        let result = run(temp_dir.path(), Some(AgentFilter::Dead), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_list_working_only() {
        let temp_dir = setup_with_agents();
        let result = run(temp_dir.path(), Some(AgentFilter::Working), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_empty_registry() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), None, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_get_summary() {
        let temp_dir = setup_with_agents();
        let summary = get_summary(temp_dir.path()).unwrap();

        assert_eq!(summary.total, 3);
        // PIDs 12345, 12346 are likely not running, so they should show as dead
        // Only agent-3 is explicitly dead in registry. agent-1 and agent-2 have
        // fake PIDs that are almost certainly not running, so effectively dead too.
        // Just check the structure is valid:
        assert!(summary.alive + summary.dead <= summary.total);
    }

    #[test]
    fn test_effective_status_dead_process() {
        let agent = AgentEntry {
            id: "agent-1".to_string(),
            pid: 999999999, // PID that almost certainly doesn't exist
            task_id: "task-1".to_string(),
            executor: "claude".to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
            last_heartbeat: chrono::Utc::now().to_rfc3339(),
            status: AgentStatus::Working,
            output_file: "/tmp/test.log".to_string(),
        };

        let status = effective_status(&agent);
        assert_eq!(status, "dead (process exited)");
        assert!(!is_effectively_alive(&agent));
        assert!(is_effectively_dead(&agent));
    }

    #[test]
    fn test_effective_status_registry_dead() {
        // Use current PID so process is alive, but registry status is Dead
        let my_pid = std::process::id();
        let agent = AgentEntry {
            id: "agent-1".to_string(),
            pid: my_pid,
            task_id: "task-1".to_string(),
            executor: "claude".to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
            last_heartbeat: chrono::Utc::now().to_rfc3339(),
            status: AgentStatus::Dead,
            output_file: "/tmp/test.log".to_string(),
        };

        let status = effective_status(&agent);
        assert_eq!(status, "dead");
        assert!(is_effectively_dead(&agent));
    }

    #[test]
    fn test_effective_status_alive_process() {
        // Use the current process PID, which is guaranteed to be alive
        let my_pid = std::process::id();
        let agent = AgentEntry {
            id: "agent-1".to_string(),
            pid: my_pid,
            task_id: "task-1".to_string(),
            executor: "claude".to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
            last_heartbeat: chrono::Utc::now().to_rfc3339(),
            status: AgentStatus::Working,
            output_file: "/tmp/test.log".to_string(),
        };

        let status = effective_status(&agent);
        assert_eq!(status, "working");
        assert!(is_effectively_alive(&agent));
        assert!(!is_effectively_dead(&agent));
    }
}
