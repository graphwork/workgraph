//! Coordinator command - auto-spawns agents on ready tasks
//!
//! Usage:
//!   wg coordinator                    # Run loop (uses config.toml settings)
//!   wg coordinator --once             # Spawn once and exit
//!   wg coordinator --interval 60      # Poll every 60s (overrides config)
//!   wg coordinator --max-agents 4     # Limit parallel agents (overrides config)
//!   wg coordinator --install-service  # Generate systemd user service

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use std::thread;
use std::time::Duration;

use workgraph::config::Config;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::ready_tasks;
use workgraph::service::registry::{AgentRegistry, AgentStatus};

use super::dead_agents::is_process_alive;

use super::{graph_path, spawn};

/// Run the coordinator loop
/// CLI arguments override config values if provided
pub fn run(
    dir: &Path,
    cli_interval: Option<u64>,
    cli_max_agents: Option<usize>,
    cli_executor: Option<&str>,
    once: bool,
    install_service: bool,
) -> Result<()> {
    // Load config for defaults
    let config = Config::load(dir)?;

    // CLI args override config values when explicitly provided
    let interval = cli_interval.unwrap_or(config.coordinator.interval);
    let max_agents = cli_max_agents.unwrap_or(config.coordinator.max_agents);
    let executor = cli_executor
        .map(|s| s.to_string())
        .unwrap_or_else(|| config.coordinator.executor.clone());

    if install_service {
        return generate_systemd_service(dir);
    }

    let graph_path = graph_path(dir);
    if !graph_path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    println!("Coordinator starting (interval: {}s, max agents: {}, executor: {})",
             interval, max_agents, &executor);

    loop {
        if let Err(e) = coordinator_tick(dir, max_agents, &executor) {
            eprintln!("Coordinator tick error: {}", e);
        }

        if once {
            println!("Single run complete.");
            break;
        }

        thread::sleep(Duration::from_secs(interval));
    }

    Ok(())
}

/// Single coordinator tick: spawn agents on ready tasks
fn coordinator_tick(dir: &Path, max_agents: usize, executor: &str) -> Result<()> {
    let graph_path = graph_path(dir);

    // First, clean up completed agents by checking actual process status
    let finished_agents = cleanup_finished_agents(dir, &graph_path)?;
    if !finished_agents.is_empty() {
        println!("[coordinator] Cleaned up {} finished agent(s): {:?}", finished_agents.len(), finished_agents);
    }

    // Now count truly alive agents (process still running)
    let registry = AgentRegistry::load(dir)?;
    let alive_count = registry.agents.values()
        .filter(|a| a.is_alive() && is_process_alive(a.pid))
        .count();

    if alive_count >= max_agents {
        println!("[coordinator] Max agents ({}) running, waiting...", max_agents);
        return Ok(());
    }

    // Get ready tasks
    let graph = load_graph(&graph_path).context("Failed to load graph")?;
    let ready = ready_tasks(&graph);
    let slots_available = max_agents.saturating_sub(alive_count);

    if ready.is_empty() {
        let done = graph.tasks().filter(|t| t.status == Status::Done).count();
        let total = graph.tasks().count();
        if done == total && total > 0 {
            println!("[coordinator] All {} tasks complete!", total);
        } else {
            println!("[coordinator] No ready tasks (done: {}/{})", done, total);
        }
        return Ok(());
    }

    // Spawn agents on ready tasks
    let to_spawn = ready.iter().take(slots_available);
    for task in to_spawn {
        // Skip if already claimed
        if task.assigned.is_some() {
            continue;
        }

        println!("[coordinator] Spawning agent for: {} - {}", task.id, task.title);
        match spawn::spawn_agent(dir, &task.id, executor, None) {
            Ok((agent_id, pid)) => {
                println!("[coordinator] Spawned {} (PID {})", agent_id, pid);
            }
            Err(e) => {
                eprintln!("[coordinator] Failed to spawn for {}: {}", task.id, e);
            }
        }
    }

    Ok(())
}

/// Clean up agents whose processes have exited
/// Returns list of cleaned up agent IDs
fn cleanup_finished_agents(dir: &Path, graph_path: &Path) -> Result<Vec<String>> {
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    // Find agents that are marked alive but process is gone
    let finished: Vec<_> = locked_registry.agents.values()
        .filter(|a| a.is_alive() && !is_process_alive(a.pid))
        .map(|a| (a.id.clone(), a.task_id.clone(), a.pid))
        .collect();

    if finished.is_empty() {
        return Ok(vec![]);
    }

    // Mark these agents as done in registry
    for (agent_id, _, _) in &finished {
        if let Some(agent) = locked_registry.get_agent_mut(agent_id) {
            agent.status = AgentStatus::Done;
        }
    }
    locked_registry.save_ref()?;

    // Unclaim their tasks (if still in progress - agent may have completed or failed them already)
    let mut graph = load_graph(graph_path).context("Failed to load graph")?;
    let mut tasks_modified = false;

    for (agent_id, task_id, pid) in &finished {
        if let Some(task) = graph.get_task_mut(task_id) {
            // Only unclaim if task is still in progress (agent didn't finish it properly)
            if task.status == Status::InProgress {
                task.status = Status::Open;
                task.assigned = None;
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: None,
                    message: format!(
                        "Task unclaimed: agent '{}' (PID {}) process exited",
                        agent_id, pid
                    ),
                });
                tasks_modified = true;
            }
        }
    }

    if tasks_modified {
        save_graph(&graph, graph_path).context("Failed to save graph")?;
    }

    Ok(finished.into_iter().map(|(id, _, _)| id).collect())
}

/// Generate systemd user service file
/// Uses config.toml for settings, so ExecStart is just 'wg coordinator'
fn generate_systemd_service(dir: &Path) -> Result<()> {
    let workdir = dir.canonicalize()
        .unwrap_or_else(|_| dir.to_path_buf());

    // Simple ExecStart - settings come from .workgraph/config.toml
    let service_content = format!(r#"[Unit]
Description=Workgraph Coordinator
After=network.target

[Service]
Type=simple
WorkingDirectory={workdir}
ExecStart={wg} coordinator
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
"#,
        workdir = workdir.display(),
        wg = std::env::current_exe()?.display(),
    );

    // Write to ~/.config/systemd/user/wg-coordinator.service
    let home = std::env::var("HOME").context("HOME not set")?;
    let service_dir = std::path::PathBuf::from(&home)
        .join(".config")
        .join("systemd")
        .join("user");

    std::fs::create_dir_all(&service_dir)?;

    let service_path = service_dir.join("wg-coordinator.service");
    std::fs::write(&service_path, service_content)?;

    println!("Created systemd user service: {}", service_path.display());
    println!();
    println!("Settings are read from .workgraph/config.toml");
    println!("To change settings: wg config --max-agents N --interval N");
    println!();
    println!("To enable and start:");
    println!("  systemctl --user daemon-reload");
    println!("  systemctl --user enable wg-coordinator");
    println!("  systemctl --user start wg-coordinator");
    println!();
    println!("To check status:");
    println!("  systemctl --user status wg-coordinator");
    println!("  journalctl --user -u wg-coordinator -f");

    Ok(())
}
