//! Coordinator command - auto-spawns agents on ready tasks
//!
//! Usage:
//!   wg coordinator                    # Run loop (uses config.toml settings)
//!   wg coordinator --once             # Spawn once and exit
//!   wg coordinator --interval 60      # Poll every 60s (overrides config)
//!   wg coordinator --max-agents 4     # Limit parallel agents (overrides config)
//!   wg coordinator --install-service  # Generate systemd user service

use anyhow::{Context, Result};
use std::path::Path;
use std::thread;
use std::time::Duration;

use workgraph::config::Config;
use workgraph::parser::load_graph;
use workgraph::query::ready_tasks;
use workgraph::service::registry::AgentRegistry;

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
    let graph = load_graph(&graph_path).context("Failed to load graph")?;

    // Count current active agents
    let registry = AgentRegistry::load(dir)?;
    let alive_count = registry.agents.values()
        .filter(|a| a.is_alive())
        .count();

    if alive_count >= max_agents {
        println!("[coordinator] Max agents ({}) running, waiting...", max_agents);
        return Ok(());
    }

    // Clean up dead agents
    let dead_agents: Vec<_> = registry.agents.iter()
        .filter(|(_, a)| !a.is_alive())
        .map(|(id, _)| id.clone())
        .collect();

    if !dead_agents.is_empty() {
        println!("[coordinator] Cleaning up {} dead agents", dead_agents.len());
        // Dead agent cleanup is handled by dead_agents command
        // For now just report
    }

    // Get ready tasks
    let ready = ready_tasks(&graph);
    let slots_available = max_agents.saturating_sub(alive_count);

    if ready.is_empty() {
        let done = graph.tasks().filter(|t| t.status == workgraph::graph::Status::Done).count();
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
