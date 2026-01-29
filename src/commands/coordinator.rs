//! Coordinator command - auto-spawns agents on ready tasks
//!
//! Usage:
//!   wg coordinator                    # Run loop
//!   wg coordinator --once             # Spawn once and exit
//!   wg coordinator --interval 60      # Poll every 60s
//!   wg coordinator --max-agents 4     # Limit parallel agents
//!   wg coordinator --install-service  # Generate systemd user service

use anyhow::{Context, Result};
use std::path::Path;
use std::thread;
use std::time::Duration;

use workgraph::parser::load_graph;
use workgraph::query::ready_tasks;
use workgraph::service::registry::AgentRegistry;

use super::{graph_path, spawn};

/// Run the coordinator loop
pub fn run(
    dir: &Path,
    interval: u64,
    max_agents: usize,
    executor: &str,
    once: bool,
    install_service: bool,
) -> Result<()> {
    if install_service {
        return generate_systemd_service(dir, interval, max_agents, executor);
    }

    let graph_path = graph_path(dir);
    if !graph_path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    println!("Coordinator starting (interval: {}s, max agents: {}, executor: {})",
             interval, max_agents, executor);

    loop {
        if let Err(e) = coordinator_tick(dir, max_agents, executor) {
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
fn generate_systemd_service(
    dir: &Path,
    interval: u64,
    max_agents: usize,
    executor: &str,
) -> Result<()> {
    let workdir = dir.canonicalize()
        .unwrap_or_else(|_| dir.to_path_buf());

    let service_content = format!(r#"[Unit]
Description=Workgraph Coordinator
After=network.target

[Service]
Type=simple
WorkingDirectory={workdir}
ExecStart={wg} coordinator --interval {interval} --max-agents {max_agents} --executor {executor}
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
"#,
        workdir = workdir.display(),
        wg = std::env::current_exe()?.display(),
        interval = interval,
        max_agents = max_agents,
        executor = executor,
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
