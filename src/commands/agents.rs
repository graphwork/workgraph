//! List running agents
//!
//! Displays information about all agents registered in the service registry.
//!
//! Usage:
//!   wg agents              # List all agents in table format
//!   wg agents --json       # Output as JSON for scripting
//!   wg agents --alive      # Only show alive agents
//!   wg agents --dead       # Only show dead agents

use anyhow::Result;
use std::path::Path;
use workgraph::service::{AgentRegistry, AgentStatus};

/// List all agents in the registry
pub fn run(dir: &Path, filter: Option<AgentFilter>, json: bool) -> Result<()> {
    let registry = AgentRegistry::load(dir)?;
    let agents = registry.list_agents();

    // Apply filter
    let filtered: Vec<_> = match filter {
        Some(AgentFilter::Alive) => agents.into_iter().filter(|a| a.is_alive()).collect(),
        Some(AgentFilter::Dead) => agents
            .into_iter()
            .filter(|a| a.status == AgentStatus::Dead)
            .collect(),
        Some(AgentFilter::Working) => agents
            .into_iter()
            .filter(|a| a.status == AgentStatus::Working)
            .collect(),
        Some(AgentFilter::Idle) => agents
            .into_iter()
            .filter(|a| a.status == AgentStatus::Idle)
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

fn output_json(agents: &[&workgraph::service::AgentEntry]) -> Result<()> {
    let output: Vec<_> = agents
        .iter()
        .map(|a| {
            serde_json::json!({
                "id": a.id,
                "task_id": a.task_id,
                "executor": a.executor,
                "pid": a.pid,
                "started_at": a.started_at,
                "last_heartbeat": a.last_heartbeat,
                "uptime": a.uptime_human(),
                "uptime_secs": a.uptime_secs(),
                "status": format!("{:?}", a.status).to_lowercase(),
                "output_file": a.output_file,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn output_table(agents: &[&workgraph::service::AgentEntry]) -> Result<()> {
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

        let status_display = match agent.status {
            AgentStatus::Starting => "starting",
            AgentStatus::Working => "working",
            AgentStatus::Idle => "idle",
            AgentStatus::Stopping => "stopping",
            AgentStatus::Done => "done",
            AgentStatus::Failed => "failed",
            AgentStatus::Dead => "dead",
        };

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

    // Summary
    let alive_count = agents.iter().filter(|a| a.is_alive()).count();
    let dead_count = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Dead)
        .count();

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
pub fn get_summary(dir: &Path) -> Result<AgentSummary> {
    let registry = AgentRegistry::load(dir)?;
    let agents = registry.list_agents();

    let total = agents.len();
    let alive = agents.iter().filter(|a| a.is_alive()).count();
    let working = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Working)
        .count();
    let idle = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Idle)
        .count();
    let dead = agents
        .iter()
        .filter(|a| a.status == AgentStatus::Dead)
        .count();

    Ok(AgentSummary {
        total,
        alive,
        working,
        idle,
        dead,
    })
}

/// Summary of agent counts
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
        registry.update_status("agent-1", AgentStatus::Working).unwrap();
        registry.update_status("agent-2", AgentStatus::Idle).unwrap();
        registry.update_status("agent-3", AgentStatus::Dead).unwrap();

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
        assert_eq!(summary.alive, 2); // Working + Idle
        assert_eq!(summary.working, 1);
        assert_eq!(summary.idle, 1);
        assert_eq!(summary.dead, 1);
    }
}
