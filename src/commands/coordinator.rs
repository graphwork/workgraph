//! Coordinator command - DEPRECATED in favor of `wg service start`
//!
//! The coordinator loop is now integrated into the service daemon.
//! This command is kept for backwards compatibility:
//!   wg coordinator --once             # Run a single coordinator tick (debug mode)
//!   wg coordinator --install-service  # DEPRECATED: use `wg service install` instead
//!   wg coordinator [other flags]      # DEPRECATED: use `wg service start` instead

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;

use workgraph::config::Config;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::ready_tasks;
use workgraph::service::registry::{AgentRegistry, AgentEntry, AgentStatus};

use super::dead_agents::is_process_alive;

use super::{graph_path, spawn};

/// Run the coordinator command (deprecated - delegates to service)
///
/// - `--once`: Run a single coordinator tick and exit (kept for debugging)
/// - `--install-service`: Deprecated, points to `wg service install`
/// - All other invocations: Deprecated, points to `wg service start`
pub fn run(
    dir: &Path,
    _cli_interval: Option<u64>,
    cli_max_agents: Option<usize>,
    cli_executor: Option<&str>,
    once: bool,
    install_service: bool,
) -> Result<()> {
    if install_service {
        eprintln!("DEPRECATED: 'wg coordinator --install-service' is deprecated.");
        eprintln!("Use 'wg service install' instead.");
        eprintln!();
        // Still run it for now, but through the new location
        return generate_systemd_service(dir);
    }

    if once {
        // Single-tick debug mode: kept as-is
        let config = Config::load(dir)?;
        let max_agents = cli_max_agents.unwrap_or(config.coordinator.max_agents);
        let executor = cli_executor
            .map(|s| s.to_string())
            .unwrap_or_else(|| config.coordinator.executor.clone());

        let graph_path = graph_path(dir);
        if !graph_path.exists() {
            anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
        }

        let model = config.coordinator.model.clone();
        println!("Running single coordinator tick (max_agents={}, executor={}, model={})...",
                 max_agents, &executor, model.as_deref().unwrap_or("default"));
        match coordinator_tick(dir, max_agents, &executor, model.as_deref()) {
            Ok(result) => {
                println!("Tick complete: {} alive, {} ready, {} spawned",
                         result.agents_alive, result.tasks_ready, result.agents_spawned);
            }
            Err(e) => eprintln!("Coordinator tick error: {}", e),
        }
        return Ok(());
    }

    // Deprecated: running the coordinator loop directly
    eprintln!("DEPRECATED: 'wg coordinator' is deprecated.");
    eprintln!("The coordinator loop is now integrated into the service daemon.");
    eprintln!();
    eprintln!("Use instead:");
    eprintln!("  wg service start              # Start the service daemon (includes coordinator)");
    eprintln!("  wg service status             # Check service status");
    eprintln!("  wg coordinator --once         # Run a single tick for debugging");
    eprintln!();
    eprintln!("To install as a systemd service:");
    eprintln!("  wg service install            # Generate systemd user service file");
    std::process::exit(1);
}

/// Result of a single coordinator tick
pub struct TickResult {
    /// Number of agents alive after the tick
    pub agents_alive: usize,
    /// Number of ready tasks found
    pub tasks_ready: usize,
    /// Number of agents spawned in this tick
    pub agents_spawned: usize,
}

/// Single coordinator tick: spawn agents on ready tasks
pub fn coordinator_tick(dir: &Path, max_agents: usize, executor: &str, model: Option<&str>) -> Result<TickResult> {
    let graph_path = graph_path(dir);

    // Load config for heartbeat timeout
    let config = Config::load(dir).unwrap_or_default();
    let heartbeat_timeout_secs = (config.agent.heartbeat_timeout * 60) as i64;

    // Clean up dead agents: process exited OR heartbeat stale
    let finished_agents = cleanup_dead_agents(dir, &graph_path, heartbeat_timeout_secs)?;
    if !finished_agents.is_empty() {
        eprintln!("[coordinator] Cleaned up {} dead agent(s): {:?}", finished_agents.len(), finished_agents);
    }

    // Now count truly alive agents (process still running and heartbeat fresh)
    let registry = AgentRegistry::load(dir)?;
    let alive_count = registry.agents.values()
        .filter(|a| a.is_alive() && is_process_alive(a.pid))
        .count();

    if alive_count >= max_agents {
        eprintln!("[coordinator] Max agents ({}) running, waiting...", max_agents);
        return Ok(TickResult { agents_alive: alive_count, tasks_ready: 0, agents_spawned: 0 });
    }

    // Get ready tasks
    let graph = load_graph(&graph_path).context("Failed to load graph")?;
    let ready = ready_tasks(&graph);
    let tasks_ready = ready.len();
    let slots_available = max_agents.saturating_sub(alive_count);

    if ready.is_empty() {
        let done = graph.tasks().filter(|t| t.status == Status::Done).count();
        let total = graph.tasks().count();
        if done == total && total > 0 {
            eprintln!("[coordinator] All {} tasks complete!", total);
        } else {
            eprintln!("[coordinator] No ready tasks (done: {}/{})", done, total);
        }
        return Ok(TickResult { agents_alive: alive_count, tasks_ready: 0, agents_spawned: 0 });
    }

    // Spawn agents on ready tasks
    let mut spawned = 0;
    let to_spawn = ready.iter().take(slots_available);
    for task in to_spawn {
        // Skip if already claimed
        if task.assigned.is_some() {
            continue;
        }

        eprintln!("[coordinator] Spawning agent for: {} - {}", task.id, task.title);
        match spawn::spawn_agent(dir, &task.id, executor, None, model) {
            Ok((agent_id, pid)) => {
                eprintln!("[coordinator] Spawned {} (PID {})", agent_id, pid);
                spawned += 1;
            }
            Err(e) => {
                eprintln!("[coordinator] Failed to spawn for {}: {}", task.id, e);
            }
        }
    }

    Ok(TickResult { agents_alive: alive_count + spawned, tasks_ready, agents_spawned: spawned })
}

/// Reason an agent was detected as dead
enum DeadReason {
    /// Process is no longer running
    ProcessExited,
    /// Heartbeat has gone stale
    HeartbeatStale { seconds: i64 },
}

/// Check if an agent should be considered dead
fn detect_dead_reason(agent: &AgentEntry, heartbeat_timeout_secs: i64) -> Option<DeadReason> {
    if !agent.is_alive() {
        return None;
    }

    // Process not running is the strongest signal
    if !is_process_alive(agent.pid) {
        return Some(DeadReason::ProcessExited);
    }

    // Heartbeat stale
    if let Some(secs) = agent.seconds_since_heartbeat() {
        if secs > heartbeat_timeout_secs {
            return Some(DeadReason::HeartbeatStale { seconds: secs });
        }
    }

    None
}

/// Clean up dead agents (process exited OR heartbeat stale)
/// Returns list of cleaned up agent IDs
fn cleanup_dead_agents(dir: &Path, graph_path: &Path, heartbeat_timeout_secs: i64) -> Result<Vec<String>> {
    let mut locked_registry = AgentRegistry::load_locked(dir)?;

    // Find agents that are dead: process gone OR heartbeat stale
    let dead: Vec<_> = locked_registry.agents.values()
        .filter_map(|a| {
            detect_dead_reason(a, heartbeat_timeout_secs)
                .map(|reason| (a.id.clone(), a.task_id.clone(), a.pid, reason))
        })
        .collect();

    if dead.is_empty() {
        return Ok(vec![]);
    }

    // Mark these agents as dead in registry
    for (agent_id, _, _, _) in &dead {
        if let Some(agent) = locked_registry.get_agent_mut(agent_id) {
            agent.status = AgentStatus::Dead;
        }
    }
    locked_registry.save_ref()?;

    // Unclaim their tasks (if still in progress - agent may have completed or failed them already)
    let mut graph = load_graph(graph_path).context("Failed to load graph")?;
    let mut tasks_modified = false;

    for (agent_id, task_id, pid, reason) in &dead {
        if let Some(task) = graph.get_task_mut(task_id) {
            // Only unclaim if task is still in progress (agent didn't finish it properly)
            if task.status == Status::InProgress {
                task.status = Status::Open;
                task.assigned = None;
                let reason_msg = match reason {
                    DeadReason::ProcessExited => format!(
                        "Task unclaimed: agent '{}' (PID {}) process exited",
                        agent_id, pid
                    ),
                    DeadReason::HeartbeatStale { seconds } => format!(
                        "Task unclaimed: agent '{}' (PID {}) no heartbeat for {}s",
                        agent_id, pid, seconds
                    ),
                };
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: None,
                    message: reason_msg,
                });
                tasks_modified = true;
            }
        }
    }

    if tasks_modified {
        save_graph(&graph, graph_path).context("Failed to save graph")?;
    }

    Ok(dead.into_iter().map(|(id, _, _, _)| id).collect())
}

/// Generate systemd user service file
/// Uses `wg service start` as ExecStart; settings come from config.toml
pub fn generate_systemd_service(dir: &Path) -> Result<()> {
    let workdir = dir.canonicalize()
        .unwrap_or_else(|_| dir.to_path_buf());

    // ExecStart uses `wg service start` - the service daemon includes the coordinator
    let service_content = format!(r#"[Unit]
Description=Workgraph Service
After=network.target

[Service]
Type=simple
WorkingDirectory={workdir}
ExecStart={wg} --dir {wg_dir} service start
ExecStop={wg} --dir {wg_dir} service stop
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
"#,
        workdir = workdir.display(),
        wg = std::env::current_exe()?.display(),
        wg_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()).display(),
    );

    // Write to ~/.config/systemd/user/wg-service.service
    let home = std::env::var("HOME").context("HOME not set")?;
    let service_dir = std::path::PathBuf::from(&home)
        .join(".config")
        .join("systemd")
        .join("user");

    std::fs::create_dir_all(&service_dir)?;

    let service_path = service_dir.join("wg-service.service");
    std::fs::write(&service_path, service_content)?;

    println!("Created systemd user service: {}", service_path.display());
    println!();
    println!("Settings are read from .workgraph/config.toml");
    println!("To change settings: wg config --max-agents N --interval N");
    println!();
    println!("To enable and start:");
    println!("  systemctl --user daemon-reload");
    println!("  systemctl --user enable wg-service");
    println!("  systemctl --user start wg-service");
    println!();
    println!("To check status:");
    println!("  systemctl --user status wg-service");
    println!("  journalctl --user -u wg-service -f");

    Ok(())
}
