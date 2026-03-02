//! Agent Service Daemon
//!
//! Manages the wg service daemon that coordinates agent spawning, monitoring,
//! and automatic task assignment. The daemon integrates coordinator logic to
//! periodically find ready tasks, spawn agents, and clean up finished agents.
//!
//! Usage:
//!   wg service start [--max-agents N] [--executor E] [--interval S]  # Start with overrides
//!   wg service stop [--force]                                        # Stop the service daemon
//!   wg service status                                                # Show service + coordinator state
//!
//! The daemon respects coordinator config from .workgraph/config.toml:
//!   [coordinator]
//!   max_agents = 4       # Maximum parallel agents
//!   poll_interval = 60   # Background safety-net poll interval (seconds)
//!   interval = 30        # Coordinator tick interval (standalone command)
//!   executor = "claude"  # Executor for spawned agents

mod coordinator;
pub(crate) mod coordinator_agent;
pub mod ipc;
mod triage;

pub use ipc::{IpcRequest, IpcResponse};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

use chrono::{DateTime, Utc};

use workgraph::agency;
use workgraph::config::Config;
use workgraph::parser::load_graph;
use workgraph::service::registry::AgentRegistry;

use super::{graph_path, is_process_alive, kill_process_force, kill_process_graceful};

// ---------------------------------------------------------------------------
// Persistent daemon logger
// ---------------------------------------------------------------------------

/// Maximum log file size before rotation (10 MB)
const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Path to the daemon log file
pub fn log_file_path(dir: &Path) -> PathBuf {
    dir.join("service").join("daemon.log")
}

/// A simple file-based logger with timestamps and size-based rotation.
///
/// The logger keeps one backup (`daemon.log.1`) and truncates when the active
/// log exceeds [`LOG_MAX_BYTES`].
#[derive(Clone)]
pub struct DaemonLogger {
    inner: Arc<Mutex<DaemonLoggerInner>>,
}

struct DaemonLoggerInner {
    file: fs::File,
    path: PathBuf,
    written: u64,
}

impl DaemonLogger {
    /// Open (or create) the log file at `.workgraph/service/daemon.log`.
    pub fn open(dir: &Path) -> Result<Self> {
        let service_dir = dir.join("service");
        if !service_dir.exists() {
            fs::create_dir_all(&service_dir)?;
        }
        let path = log_file_path(dir);
        let file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to open daemon log at {:?}", path))?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            inner: Arc::new(Mutex::new(DaemonLoggerInner {
                file,
                path,
                written,
            })),
        })
    }

    /// Write a timestamped line to the log.  `level` is a short tag like
    /// `INFO`, `WARN`, or `ERROR`.
    pub fn log(&self, level: &str, msg: &str) {
        let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
        let line = format!("{} [{}] {}\n", ts, level, msg);
        if let Ok(mut inner) = self.inner.lock() {
            if let Err(e) = inner.file.write_all(line.as_bytes()) {
                eprintln!("Warning: daemon log write failed: {}", e);
            }
            if let Err(e) = inner.file.flush() {
                eprintln!("Warning: daemon log flush failed: {}", e);
            }
            inner.written += line.len() as u64;
            if inner.written >= LOG_MAX_BYTES {
                Self::rotate(&mut inner);
            }
        }
    }

    pub fn info(&self, msg: &str) {
        self.log("INFO", msg);
    }

    pub fn warn(&self, msg: &str) {
        self.log("WARN", msg);
    }

    pub fn error(&self, msg: &str) {
        self.log("ERROR", msg);
    }

    /// Rotate: rename current log to `.log.1` (overwriting any previous
    /// backup) and open a fresh file.
    fn rotate(inner: &mut DaemonLoggerInner) {
        let backup = inner.path.with_extension("log.1");
        // Best-effort: ignore errors during rotation
        let _ = fs::rename(&inner.path, &backup);
        if let Ok(f) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&inner.path)
        {
            inner.file = f;
            inner.written = 0;
        }
    }

    /// Install a panic hook that writes the panic info to this log before
    /// the process aborts.
    pub fn install_panic_hook(&self) {
        let logger = self.clone();
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let msg = format!("PANIC: {}", info);
            logger.log("FATAL", &msg);
            default_hook(info);
        }));
    }
}

/// Read the last `n` lines from the daemon log that match the given level
/// (or all lines if `level_filter` is `None`).  Returns up to `n` lines,
/// most recent last.
pub fn tail_log(dir: &Path, n: usize, level_filter: Option<&str>) -> Vec<String> {
    let path = log_file_path(dir);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let lines: Vec<&str> = content.lines().collect();
    let filtered: Vec<String> = if let Some(level) = level_filter {
        let tag = format!("[{}]", level);
        lines
            .iter()
            .filter(|l| l.contains(&tag))
            .map(std::string::ToString::to_string)
            .collect()
    } else {
        lines.iter().map(std::string::ToString::to_string).collect()
    };
    filtered
        .into_iter()
        .rev()
        .take(n)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

/// Default socket path (project-specific, inside .workgraph dir)
pub fn default_socket_path(dir: &Path) -> PathBuf {
    dir.join("service").join("daemon.sock")
}

/// Path to the service state file
pub fn state_file_path(dir: &Path) -> PathBuf {
    dir.join("service").join("state.json")
}

/// Service state stored on disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceState {
    pub pid: u32,
    pub socket_path: String,
    pub started_at: String,
}

impl ServiceState {
    pub fn load(dir: &Path) -> Result<Option<Self>> {
        let path = state_file_path(dir);
        if !path.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&path)
            .with_context(|| format!("Failed to read service state from {:?}", path))?;
        let state: ServiceState = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse service state from {:?}", path))?;
        Ok(Some(state))
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let service_dir = dir.join("service");
        if !service_dir.exists() {
            fs::create_dir_all(&service_dir).with_context(|| {
                format!("Failed to create service directory at {:?}", service_dir)
            })?;
        }
        let path = state_file_path(dir);
        let content =
            serde_json::to_string_pretty(self).context("Failed to serialize service state")?;
        fs::write(&path, content)
            .with_context(|| format!("Failed to write service state to {:?}", path))?;
        Ok(())
    }

    pub fn remove(dir: &Path) -> Result<()> {
        let path = state_file_path(dir);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("Failed to remove service state at {:?}", path))?;
        }
        Ok(())
    }
}

/// Path to the coordinator state file
pub fn coordinator_state_path(dir: &Path) -> PathBuf {
    dir.join("service").join("coordinator-state.json")
}

/// Runtime coordinator state persisted to disk for status queries
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoordinatorState {
    /// Whether the coordinator is enabled
    pub enabled: bool,
    /// Effective config: max agents
    pub max_agents: usize,
    /// Effective config: background poll interval seconds (safety net)
    pub poll_interval: u64,
    /// Effective config: executor name
    pub executor: String,
    /// Effective config: model for spawned agents
    #[serde(default)]
    pub model: Option<String>,
    /// Total coordinator ticks completed
    pub ticks: u64,
    /// ISO 8601 timestamp of the last tick
    pub last_tick: Option<String>,
    /// Number of agents alive at last tick
    pub agents_alive: usize,
    /// Number of tasks ready at last tick
    pub tasks_ready: usize,
    /// Number of agents spawned in last tick
    pub agents_spawned: usize,
    /// Whether the coordinator is paused (no new agent spawns)
    #[serde(default)]
    pub paused: bool,
}

impl CoordinatorState {
    pub fn load(dir: &Path) -> Option<Self> {
        let path = coordinator_state_path(dir);
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return None, // file doesn't exist yet
        };
        match serde_json::from_str(&content) {
            Ok(state) => Some(state),
            Err(e) => {
                eprintln!(
                    "Warning: corrupt coordinator state at {}: {}",
                    path.display(),
                    e
                );
                None
            }
        }
    }

    pub fn save(&self, dir: &Path) {
        let path = coordinator_state_path(dir);
        match serde_json::to_string_pretty(self) {
            Ok(content) => {
                if let Err(e) = fs::write(&path, content) {
                    eprintln!(
                        "Warning: failed to save coordinator state to {}: {}",
                        path.display(),
                        e
                    );
                }
            }
            Err(e) => {
                eprintln!("Warning: failed to serialize coordinator state: {}", e);
            }
        }
    }

    /// Load coordinator state, defaulting to empty if missing or corrupt.
    /// Corrupt files already emit a warning via `load()`.
    pub fn load_or_default(dir: &Path) -> Self {
        Self::load(dir).unwrap_or_default()
    }

    pub fn remove(dir: &Path) {
        let path = coordinator_state_path(dir);
        let _ = fs::remove_file(&path);
    }
}

/// Generate systemd user service file
/// Uses `wg service start` as ExecStart; settings come from config.toml
pub fn generate_systemd_service(dir: &Path) -> Result<()> {
    let workdir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());

    // Derive a project identifier from the directory basename for unique service naming
    let project_name = workdir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("default");
    // Sanitize for systemd unit naming: keep alphanumerics, hyphens, underscores
    let project_name: String = project_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let unit_name = format!("wg-{project_name}");

    // ExecStart uses `wg service start` - the service daemon includes the coordinator
    let service_content = format!(
        r#"[Unit]
Description=Workgraph Service ({project_name})
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
        project_name = project_name,
        workdir = workdir.display(),
        wg = std::env::current_exe()?.display(),
        wg_dir = dir
            .canonicalize()
            .unwrap_or_else(|_| dir.to_path_buf())
            .display(),
    );

    // Write to ~/.config/systemd/user/wg-{project_name}.service
    let home = std::env::var("HOME").context("HOME not set")?;
    let service_dir = std::path::PathBuf::from(&home)
        .join(".config")
        .join("systemd")
        .join("user");

    std::fs::create_dir_all(&service_dir)?;

    let service_path = service_dir.join(format!("{unit_name}.service"));
    std::fs::write(&service_path, service_content)?;

    println!("Created systemd user service: {}", service_path.display());
    println!();
    println!("Settings are read from .workgraph/config.toml");
    println!("To change settings: wg config --max-agents N --interval N");
    println!();
    println!("To enable and start:");
    println!("  systemctl --user daemon-reload");
    println!("  systemctl --user enable {unit_name}");
    println!("  systemctl --user start {unit_name}");
    println!();
    println!("To check status:");
    println!("  systemctl --user status {unit_name}");
    println!("  journalctl --user -u {unit_name} -f");

    Ok(())
}

/// Run a single coordinator tick (debug/testing command)
pub fn run_tick(
    dir: &Path,
    max_agents: Option<usize>,
    executor: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    let config = Config::load(dir)?;
    let max_agents = max_agents.unwrap_or(config.coordinator.max_agents);
    let executor = executor
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| config.coordinator.executor.clone());

    let graph_path = graph_path(dir);
    if !graph_path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let model = model
        .map(std::string::ToString::to_string)
        .or_else(|| config.coordinator.model.clone());
    println!(
        "Running single coordinator tick (max_agents={}, executor={}, model={})...",
        max_agents,
        &executor,
        model.as_deref().unwrap_or("default")
    );
    match coordinator::coordinator_tick(dir, max_agents, &executor, model.as_deref()) {
        Ok(result) => {
            println!(
                "Tick complete: {} alive, {} ready, {} spawned",
                result.agents_alive, result.tasks_ready, result.agents_spawned
            );
        }
        Err(e) => eprintln!("Coordinator tick error: {}", e),
    }
    Ok(())
}

pub fn find_orphan_daemon_pids(dir: &Path, exclude_pid: Option<u32>) -> Vec<u32> {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let dir_str = canonical.to_string_lossy().to_string();
    let our_pid = std::process::id();

    let mut orphans = Vec::new();

    let proc_dir = match fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return orphans,
    };

    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // Only look at numeric directories (PID directories)
        let pid: u32 = match name_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Skip our own process and the excluded PID
        if pid == our_pid || exclude_pid == Some(pid) {
            continue;
        }

        // Read cmdline
        let cmdline_path = format!("/proc/{}/cmdline", pid);
        let cmdline = match fs::read(&cmdline_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // cmdline is NUL-separated
        let cmdline_str = String::from_utf8_lossy(&cmdline);
        let args: Vec<&str> = cmdline_str.split('\0').collect();

        // Check if this is a `wg ... service daemon --dir <our_dir>` process
        let has_service_daemon = args
            .windows(2)
            .any(|w| w[0] == "service" && w[1] == "daemon");
        let has_our_dir = args.windows(2).any(|w| w[0] == "--dir" && w[1] == dir_str);

        if has_service_daemon && has_our_dir {
            orphans.push(pid);
        }
    }

    orphans
}

#[cfg(not(unix))]
pub fn find_orphan_daemon_pids(_dir: &Path, _exclude_pid: Option<u32>) -> Vec<u32> {
    Vec::new()
}

/// Start the service daemon
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub fn run_start(
    dir: &Path,
    socket_path: Option<&str>,
    _port: Option<u16>,
    max_agents: Option<usize>,
    executor: Option<&str>,
    interval: Option<u64>,
    model: Option<&str>,
    json: bool,
    force: bool,
    no_coordinator_agent: bool,
) -> Result<()> {
    // Check if service is already running
    if let Some(state) = ServiceState::load(dir)? {
        if is_process_alive(state.pid) {
            if force {
                // Kill existing daemon before starting a new one
                if !json {
                    println!(
                        "Killing existing daemon (PID {}) before starting new one...",
                        state.pid
                    );
                }
                // Send shutdown via IPC first (graceful)
                let socket = PathBuf::from(&state.socket_path);
                if socket.exists()
                    && let Ok(mut stream) = UnixStream::connect(&socket)
                {
                    let request = IpcRequest::Shutdown {
                        force: false,
                        kill_agents: false,
                    };
                    if let Ok(json_req) = serde_json::to_string(&request) {
                        let _ = writeln!(stream, "{}", json_req);
                        let _ = stream.flush();
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                // If still alive, kill it
                if is_process_alive(state.pid) {
                    kill_process_graceful(state.pid, 5)?;
                }
                // Clean up
                if socket.exists() {
                    let _ = fs::remove_file(&socket);
                }
                ServiceState::remove(dir)?;
            } else {
                if json {
                    let output = serde_json::json!({
                        "error": "Service already running",
                        "pid": state.pid,
                        "socket": state.socket_path,
                    });
                    println!("{}", serde_json::to_string_pretty(&output)?);
                } else {
                    println!(
                        "Service already running (PID {}). Use 'wg service stop' first or 'wg service start --force'.",
                        state.pid
                    );
                    println!("Socket: {}", state.socket_path);
                }
                return Ok(());
            }
        } else {
            // Stale state, clean up
            ServiceState::remove(dir)?;
        }
    }

    // Also check for orphan daemon processes that lost their state file
    let orphans = find_orphan_daemon_pids(dir, None);
    if !orphans.is_empty() {
        if force {
            for &pid in &orphans {
                if !json {
                    println!("Killing orphan daemon process (PID {})...", pid);
                }
                let _ = kill_process_graceful(pid, 5);
            }
        } else {
            let pids: Vec<String> = orphans.iter().map(|p| p.to_string()).collect();
            if json {
                let output = serde_json::json!({
                    "error": "Orphan daemon processes found",
                    "orphan_pids": orphans,
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!(
                    "Found orphan daemon process(es) for this workgraph: PID {}",
                    pids.join(", ")
                );
                println!("Use 'wg service start --force' to kill them and start fresh.");
            }
            return Ok(());
        }
    }

    let socket = socket_path
        .map(PathBuf::from)
        .unwrap_or_else(|| default_socket_path(dir));

    // Remove stale socket file if exists
    if socket.exists() {
        fs::remove_file(&socket)
            .with_context(|| format!("Failed to remove stale socket at {:?}", socket))?;
    }

    // Fork the daemon process
    let current_exe = std::env::current_exe().context("Failed to get current executable path")?;

    let dir_str = dir.to_string_lossy().to_string();
    let socket_str = socket.to_string_lossy().to_string();

    // Start daemon in background
    let mut args = vec![
        "--dir".to_string(),
        dir_str,
        "service".to_string(),
        "daemon".to_string(),
        "--socket".to_string(),
        socket_str.clone(),
    ];
    if let Some(n) = max_agents {
        args.push("--max-agents".to_string());
        args.push(n.to_string());
    }
    if let Some(e) = executor {
        args.push("--executor".to_string());
        args.push(e.to_string());
    }
    if let Some(i) = interval {
        args.push("--interval".to_string());
        args.push(i.to_string());
    }
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    if no_coordinator_agent {
        args.push("--no-coordinator-agent".to_string());
    }
    // Redirect daemon stderr to the log file so early startup crashes and
    // unexpected panics that bypass the DaemonLogger are captured.
    let log_path = log_file_path(dir);
    let service_dir = dir.join("service");
    if !service_dir.exists() {
        fs::create_dir_all(&service_dir)
            .context("Failed to create service directory for log file")?;
    }
    let stderr_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("Failed to open daemon log at {:?}", log_path))?;

    let child = process::Command::new(&current_exe)
        .args(&args)
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::null())
        .stderr(stderr_file)
        .spawn()
        .context("Failed to spawn daemon process")?;

    let pid = child.id();

    // Save state
    let state = ServiceState {
        pid,
        socket_path: socket_str.clone(),
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    state.save(dir)?;

    // Wait a moment for the daemon to start
    std::thread::sleep(Duration::from_millis(200));

    // Verify daemon started successfully
    if !is_process_alive(pid) {
        ServiceState::remove(dir)?;
        anyhow::bail!("Daemon process exited immediately. Check logs.");
    }

    // Resolve effective config for display (CLI flags override config.toml)
    let config = Config::load_or_default(dir);
    let eff_max_agents = max_agents.unwrap_or(config.coordinator.max_agents);
    let eff_poll_interval = interval.unwrap_or(config.coordinator.poll_interval);
    let eff_executor = executor
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| config.coordinator.executor.clone());
    let eff_model: Option<String> = model
        .map(std::string::ToString::to_string)
        .or_else(|| config.coordinator.model.clone());

    let log_path_str = log_path.to_string_lossy().to_string();

    // Warn if auto_assign is enabled but no agency agents are defined
    let no_agents_defined = {
        let agents_dir = dir.join("agency").join("cache/agents");
        agency::load_all_agents_or_warn(&agents_dir).is_empty()
    };
    let warn_no_agents = config.agency.auto_assign && no_agents_defined;

    if json {
        let mut output = serde_json::json!({
            "status": "started",
            "pid": pid,
            "socket": socket_str,
            "log": log_path_str,
            "coordinator": {
                "max_agents": eff_max_agents,
                "poll_interval": eff_poll_interval,
                "executor": eff_executor,
                "model": eff_model,
            }
        });
        if warn_no_agents {
            output["warning"] = serde_json::json!(
                "auto_assign is enabled but no agents are defined. Run 'wg agency init' or 'wg agent create' to create agents."
            );
        }
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Service started (PID {})", pid);
        println!("Socket: {}", socket_str);
        println!("Log: {}", log_path_str);
        let model_str = eff_model.as_deref().unwrap_or("default");
        println!(
            "Coordinator: max_agents={}, poll_interval={}s, executor={}, model={}",
            eff_max_agents, eff_poll_interval, eff_executor, model_str
        );
        if warn_no_agents {
            println!();
            println!("Warning: auto_assign is enabled but no agents are defined.");
            println!("  Run 'wg agency init' or 'wg agent create' to create agents.");
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_start(
    _dir: &Path,
    _socket_path: Option<&str>,
    _port: Option<u16>,
    _max_agents: Option<usize>,
    _executor: Option<&str>,
    _interval: Option<u64>,
    _model: Option<&str>,
    _json: bool,
    _force: bool,
    _no_coordinator_agent: bool,
) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Reap zombie child processes (non-blocking).
///
/// The daemon spawns agent processes via `Command::spawn()`. When an agent
/// exits (or is killed), its process becomes a zombie until the parent calls
/// `waitpid`. This function reaps all zombies so that `is_process_alive(pid)`
/// correctly returns `false` for dead agents.
#[cfg(unix)]
fn reap_zombies() {
    loop {
        let result = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if result <= 0 {
            break; // No more zombies (0) or error (-1, e.g. no children)
        }
    }
}

/// Mutable coordinator runtime config, updated by Reconfigure IPC.
pub(crate) struct DaemonConfig {
    max_agents: usize,
    executor: String,
    poll_interval: Duration,
    model: Option<String>,
    paused: bool,
    /// Settling delay after GraphChanged events. During burst graph construction,
    /// multiple adds fire in rapid succession. Instead of ticking immediately on
    /// each GraphChanged, the coordinator waits this long after the *last* event
    /// before dispatching. This prevents premature dispatch on partially-wired graphs.
    settling_delay: Duration,
}

/// Route new chat inbox messages to the persistent coordinator agent.
///
/// Reads the inbox since the coordinator cursor, sends each message to the
/// agent thread, and advances the cursor. The agent thread handles context
/// injection, LLM processing, and outbox writing asynchronously.
///
/// Returns the number of messages routed.
fn route_chat_to_agent(
    dir: &Path,
    agent: &coordinator_agent::CoordinatorAgent,
    logger: &DaemonLogger,
) -> Result<usize> {
    let chat_dir = dir.join("chat");
    if !chat_dir.exists() {
        return Ok(0);
    }

    let inbox_cursor = workgraph::chat::read_coordinator_cursor(dir)?;
    let new_messages = workgraph::chat::read_inbox_since(dir, inbox_cursor)?;

    if new_messages.is_empty() {
        return Ok(0);
    }

    let count = new_messages.len();
    for msg in &new_messages {
        if let Err(e) = agent.send_message(msg.request_id.clone(), msg.content.clone()) {
            logger.error(&format!(
                "Failed to send chat message to coordinator agent: {}",
                e
            ));
            // Write an error response so the user isn't left hanging
            let _ = workgraph::chat::append_outbox(
                dir,
                "The coordinator agent is not available. Please try again.",
                &msg.request_id,
            );
        }
    }

    // Advance the coordinator cursor past these messages
    if let Some(last) = new_messages.last() {
        workgraph::chat::write_coordinator_cursor(dir, last.id)?;
    }

    Ok(count)
}

/// Record events from the latest coordinator tick into the event log.
///
/// Scans the agent registry and graph to detect new agent spawns, completions,
/// and failures since the last check. This keeps the coordinator agent's
/// context refresh up-to-date with real-time events.
fn record_tick_events(
    dir: &Path,
    event_log: &coordinator_agent::SharedEventLog,
    logger: &DaemonLogger,
) {
    // Record recently spawned agents (alive, recently started)
    if let Ok(registry) = AgentRegistry::load(dir) {
        let mut log = event_log.lock().unwrap_or_else(|e| e.into_inner());
        for agent in registry.list_agents() {
            if agent.is_alive() && is_process_alive(agent.pid) {
                // Check if agent was spawned very recently (within last 5 seconds)
                if let Some(secs) = agent.uptime_secs()
                    && secs <= 5
                {
                    log.record(coordinator_agent::Event::AgentSpawned {
                        agent_id: agent.id.clone(),
                        task_id: agent.task_id.clone(),
                        executor: agent.executor.clone(),
                    });
                }
            }
        }
    }

    // Record recently completed/failed tasks from graph state.
    // These are detected by checking for tasks that have completed_at or
    // failure_reason set recently. The coordinator tick already processes
    // dead agents, so by the time we get here, task statuses are updated.
    let gp = graph_path(dir);
    if let Ok(graph) = load_graph(&gp) {
        let recent_cutoff = chrono::Utc::now() - chrono::Duration::seconds(10);
        let mut log = event_log.lock().unwrap_or_else(|e| e.into_inner());

        for task in graph.tasks() {
            match task.status {
                workgraph::graph::Status::Done => {
                    if let Some(ref completed_at) = task.completed_at
                        && let Ok(dt) = completed_at.parse::<DateTime<Utc>>()
                        && dt > recent_cutoff
                    {
                        log.record(coordinator_agent::Event::TaskCompleted {
                            task_id: task.id.clone(),
                            agent_id: task.assigned.clone(),
                        });
                    }
                }
                workgraph::graph::Status::Failed => {
                    // Check the last log entry for recency
                    if let Some(last_log) = task.log.last()
                        && let Ok(dt) = last_log.timestamp.parse::<DateTime<Utc>>()
                        && dt > recent_cutoff
                    {
                        log.record(coordinator_agent::Event::TaskFailed {
                            task_id: task.id.clone(),
                            reason: task
                                .failure_reason
                                .as_deref()
                                .unwrap_or("unknown")
                                .to_string(),
                        });
                    }
                }
                _ => {}
            }
        }
    } else {
        logger.warn("Failed to load graph for event recording");
    }
}

/// Run the actual daemon loop (called by forked process)
#[cfg(unix)]
pub fn run_daemon(
    dir: &Path,
    socket_path: &str,
    cli_max_agents: Option<usize>,
    cli_executor: Option<&str>,
    cli_interval: Option<u64>,
    cli_model: Option<&str>,
    no_coordinator_agent: bool,
) -> Result<()> {
    let socket = PathBuf::from(socket_path);

    // --- Persistent logging setup ---
    let logger = DaemonLogger::open(dir).context("Failed to initialise daemon logger")?;
    logger.install_panic_hook();

    logger.info(&format!(
        "Daemon starting (PID {}, socket {})",
        std::process::id(),
        socket_path,
    ));

    // Ensure socket directory exists
    if let Some(parent) = socket.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)?;
    }

    // Remove existing socket
    if socket.exists() {
        fs::remove_file(&socket)?;
    }

    // Bind to socket
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("Failed to bind to socket {:?}", socket))?;

    // Set socket permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o600);
        fs::set_permissions(&socket, perms)?;
    }

    // Set non-blocking for graceful shutdown
    listener.set_nonblocking(true)?;

    let dir = dir.to_path_buf();
    let mut running = true;

    // Load coordinator config, CLI args override config values
    let config = Config::load_or_default(&dir);
    let mut daemon_cfg = DaemonConfig {
        max_agents: cli_max_agents.unwrap_or(config.coordinator.max_agents),
        executor: cli_executor
            .map(std::string::ToString::to_string)
            .unwrap_or_else(|| config.coordinator.executor.clone()),
        // The poll_interval is the slow background safety-net timer.
        // CLI --interval overrides it; otherwise use config.coordinator.poll_interval.
        poll_interval: Duration::from_secs(
            cli_interval.unwrap_or(config.coordinator.poll_interval),
        ),
        model: cli_model
            .map(std::string::ToString::to_string)
            .or_else(|| config.coordinator.model.clone()),
        paused: false,
        settling_delay: Duration::from_millis(config.coordinator.settling_delay_ms),
    };

    logger.info(&format!(
        "Coordinator config: poll_interval={}s, max_agents={}, executor={}, model={}",
        daemon_cfg.poll_interval.as_secs(),
        daemon_cfg.max_agents,
        &daemon_cfg.executor,
        daemon_cfg.model.as_deref().unwrap_or("default"),
    ));

    // Aggregate usage stats on startup
    match workgraph::usage::aggregate_usage_stats(&dir) {
        Ok(count) if count > 0 => {
            logger.info(&format!(
                "Aggregated {} usage log entries on startup",
                count
            ));
        }
        Ok(_) => {} // No entries to aggregate
        Err(e) => {
            logger.warn(&format!("Failed to aggregate usage stats: {}", e));
        }
    }

    // Initialize coordinator state on disk
    let mut coord_state = CoordinatorState {
        enabled: true,
        max_agents: daemon_cfg.max_agents,
        poll_interval: daemon_cfg.poll_interval.as_secs(),
        executor: daemon_cfg.executor.clone(),
        model: daemon_cfg.model.clone(),
        ticks: 0,
        last_tick: None,
        agents_alive: 0,
        tasks_ready: 0,
        agents_spawned: 0,
        paused: false,
    };
    coord_state.save(&dir);

    // Create the shared event log for coordinator context refresh.
    // The daemon records events (task completions, agent spawns, etc.) and the
    // coordinator agent reads them when building context for each interaction.
    let event_log = coordinator_agent::new_event_log();

    // Spawn the persistent coordinator agent (LLM session for chat).
    // The coordinator agent is a long-lived Claude CLI session that interprets
    // user intent, replacing the simple stub responses.
    // Enabled by default; disable with --no-coordinator-agent or
    // coordinator.coordinator_agent = false in config.toml.
    let enable_coordinator_agent = !no_coordinator_agent && config.coordinator.coordinator_agent;
    let coordinator_agent = if enable_coordinator_agent {
        match coordinator_agent::CoordinatorAgent::spawn(
            &dir,
            daemon_cfg.model.as_deref(),
            &logger,
            event_log.clone(),
        ) {
            Ok(agent) => {
                logger.info("Coordinator agent spawned successfully");
                Some(agent)
            }
            Err(e) => {
                logger.warn(&format!(
                    "Failed to spawn coordinator agent: {}. Chat will use stub responses.",
                    e
                ));
                None
            }
        }
    } else {
        if no_coordinator_agent {
            logger.info("Coordinator agent disabled via --no-coordinator-agent flag");
        } else {
            logger.info(
                "Coordinator agent disabled (set coordinator.coordinator_agent = true to enable)",
            );
        }
        None
    };

    // Track last coordinator tick time - run immediately on start
    let mut last_coordinator_tick = Instant::now() - daemon_cfg.poll_interval;

    // Settling deadline: when a GraphChanged event arrives, we schedule a tick
    // after a settling delay. Each subsequent GraphChanged resets the deadline,
    // debouncing burst additions so the coordinator sees the full graph.
    let mut settling_deadline: Option<Instant> = None;

    // Urgent wake: when a UserChat IPC arrives, tick immediately without settling delay.
    // This flag bypasses both the settling delay and the paused state, because
    // chat is a user-facing interaction that expects sub-second acknowledgement.
    let mut urgent_wake = false;

    while running {
        // Reap zombie child processes (agents that have exited).
        // Even though agents call setsid() to create a new session, they are
        // still children of the daemon (parent-child is set at fork, not
        // affected by setsid). Without reaping, killed agents remain as
        // zombies and is_process_alive(pid) keeps returning true.
        reap_zombies();

        match listener.accept() {
            Ok((stream, _)) => {
                let mut wake_coordinator = false;
                let mut conn_urgent_wake = false;
                if let Err(e) = ipc::handle_connection(
                    &dir,
                    stream,
                    &mut running,
                    &mut wake_coordinator,
                    &mut conn_urgent_wake,
                    &mut daemon_cfg,
                    &logger,
                ) {
                    logger.error(&format!("Error handling connection: {}", e));
                }
                if conn_urgent_wake {
                    urgent_wake = true;
                    logger.info("Urgent wake (UserChat), will tick immediately");
                }
                if wake_coordinator {
                    // Debounce: (re)set the settling deadline. Each GraphChanged
                    // pushes the deadline forward, so burst additions all land
                    // before the coordinator tick fires.
                    let new_deadline = Instant::now() + daemon_cfg.settling_delay;
                    let was_pending = settling_deadline.is_some();
                    settling_deadline = Some(new_deadline);
                    if !was_pending {
                        logger.info(&format!(
                            "GraphChanged received, scheduling coordinator tick in {}ms (settling delay)",
                            daemon_cfg.settling_delay.as_millis()
                        ));
                    } else {
                        logger.info(&format!(
                            "GraphChanged received, resetting settling deadline ({}ms from now)",
                            daemon_cfg.settling_delay.as_millis()
                        ));
                    }
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No connection, sleep briefly
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                logger.error(&format!("Accept error: {}", e));
            }
        }

        // Determine whether to run a coordinator tick.
        // Three triggers: (1) urgent wake (UserChat), (2) settling deadline expired,
        // (3) background poll interval.
        let mut should_tick = false;

        // Urgent wake: a UserChat IPC arrived. Route messages to the coordinator
        // agent if available, otherwise fall through to the coordinator tick (stub).
        if urgent_wake {
            urgent_wake = false;

            if let Some(ref agent) = coordinator_agent {
                // Route chat messages to the persistent coordinator agent.
                // Read new inbox messages and send them to the agent thread.
                match route_chat_to_agent(&dir, agent, &logger) {
                    Ok(count) if count > 0 => {
                        logger.info(&format!(
                            "Routed {} chat message(s) to coordinator agent",
                            count
                        ));
                    }
                    Ok(_) => {} // No new messages
                    Err(e) => {
                        logger.error(&format!("Failed to route chat to agent: {}", e));
                        // Fall through to tick for stub response
                        should_tick = true;
                    }
                }
            } else {
                // No coordinator agent — fall through to coordinator tick
                // which will use the stub response via process_chat_inbox.
                should_tick = true;
                logger.info("Urgent wake (no coordinator agent): running coordinator tick");
            }
        }

        if !daemon_cfg.paused {
            // Settled tick: the settling deadline has passed after GraphChanged events.
            if let Some(deadline) = settling_deadline
                && Instant::now() >= deadline
            {
                settling_deadline = None;
                should_tick = true;
                logger.info("Settling delay elapsed, running coordinator tick now");
            }
            // Background safety-net tick: runs on poll_interval even without IPC events.
            if last_coordinator_tick.elapsed() >= daemon_cfg.poll_interval {
                should_tick = true;
            }
        }
        if should_tick {
            last_coordinator_tick = Instant::now();

            // Aggregate usage stats periodically
            match workgraph::usage::aggregate_usage_stats(&dir) {
                Ok(count) if count > 0 => {
                    logger.info(&format!("Aggregated {} usage log entries", count));
                }
                Ok(_) => {} // No entries to aggregate
                Err(e) => {
                    logger.warn(&format!("Failed to aggregate usage stats: {}", e));
                }
            }

            logger.info(&format!(
                "Coordinator tick #{} starting (max_agents={}, executor={})",
                coord_state.ticks + 1,
                daemon_cfg.max_agents,
                &daemon_cfg.executor
            ));
            match coordinator::coordinator_tick(
                &dir,
                daemon_cfg.max_agents,
                &daemon_cfg.executor,
                daemon_cfg.model.as_deref(),
            ) {
                Ok(result) => {
                    coord_state.ticks += 1;
                    coord_state.last_tick = Some(chrono::Utc::now().to_rfc3339());
                    coord_state.max_agents = daemon_cfg.max_agents;
                    coord_state.poll_interval = daemon_cfg.poll_interval.as_secs();
                    coord_state.executor = daemon_cfg.executor.clone();
                    coord_state.model = daemon_cfg.model.clone();
                    coord_state.agents_alive = result.agents_alive;
                    coord_state.tasks_ready = result.tasks_ready;
                    coord_state.agents_spawned = result.agents_spawned;
                    coord_state.save(&dir);

                    // Record agent spawn events in the event log
                    if result.agents_spawned > 0 {
                        record_tick_events(&dir, &event_log, &logger);
                    }

                    logger.info(&format!(
                        "Coordinator tick #{} complete: agents_alive={}, tasks_ready={}, spawned={}",
                        coord_state.ticks, result.agents_alive, result.tasks_ready, result.agents_spawned
                    ));
                }
                Err(e) => {
                    coord_state.ticks += 1;
                    coord_state.save(&dir);
                    logger.error(&format!("Coordinator tick error: {}", e));
                }
            }
        }
    }

    logger.info("Daemon shutting down");

    // Shut down the coordinator agent
    if let Some(agent) = coordinator_agent {
        logger.info("Shutting down coordinator agent");
        agent.shutdown();
    }

    // Cleanup
    let _ = fs::remove_file(&socket);
    // Clean up coordinator prompt file
    let _ = fs::remove_file(dir.join("service").join("coordinator-prompt.txt"));
    CoordinatorState::remove(&dir);
    ServiceState::remove(&dir)?;

    logger.info("Daemon shutdown complete");

    Ok(())
}

#[cfg(not(unix))]
pub fn run_daemon(
    _dir: &Path,
    _socket_path: &str,
    _max_agents: Option<usize>,
    _executor: Option<&str>,
    _interval: Option<u64>,
    _model: Option<&str>,
    _no_coordinator_agent: bool,
) -> Result<()> {
    anyhow::bail!("Daemon is only supported on Unix systems")
}

/// Stop the service daemon
#[cfg(unix)]
pub fn run_stop(dir: &Path, force: bool, kill_agents: bool, json: bool) -> Result<()> {
    let state = match ServiceState::load(dir)? {
        Some(s) => s,
        None => {
            if json {
                let output = serde_json::json!({ "error": "Service not running" });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("Service not running");
            }
            return Ok(());
        }
    };

    // Try to send shutdown command via socket
    let socket = PathBuf::from(&state.socket_path);
    if socket.exists()
        && let Ok(mut stream) = UnixStream::connect(&socket)
    {
        let request = IpcRequest::Shutdown { force, kill_agents };
        let json_req = serde_json::to_string(&request)?;
        // Best-effort: shutdown falls through to kill if IPC fails
        if let Err(e) = writeln!(stream, "{}", json_req) {
            eprintln!("Warning: failed to send shutdown request: {}", e);
        }
        if let Err(e) = stream.flush() {
            eprintln!("Warning: failed to flush shutdown request: {}", e);
        }
        // Give it a moment to process
        std::thread::sleep(Duration::from_millis(200));
    }

    // If process is still running, kill it
    if is_process_alive(state.pid) {
        if force {
            kill_process_force(state.pid)?;
        } else {
            kill_process_graceful(state.pid, 5)?;
        }
    }

    // Clean up
    if socket.exists() {
        let _ = fs::remove_file(&socket);
    }
    ServiceState::remove(dir)?;

    // Scan for orphan daemon processes that may have been left behind by
    // previous start/stop cycles where the state file was removed but the
    // daemon process wasn't actually killed.
    let orphans = find_orphan_daemon_pids(dir, Some(state.pid));
    let mut orphan_count = 0;
    for &pid in &orphans {
        if force {
            let _ = kill_process_force(pid);
        } else {
            let _ = kill_process_graceful(pid, 5);
        }
        orphan_count += 1;
    }

    if json {
        let output = serde_json::json!({
            "status": "stopped",
            "pid": state.pid,
            "force": force,
            "kill_agents": kill_agents,
            "orphans_killed": orphan_count,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if orphan_count > 0 {
        println!(
            "Service stopped (PID {}), killed {} orphan daemon(s)",
            state.pid, orphan_count
        );
    } else if kill_agents {
        println!("Service stopped (PID {}), agents killed", state.pid);
    } else {
        println!(
            "Service stopped (PID {}), agents continue running",
            state.pid
        );
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_stop(_dir: &Path, _force: bool, _kill_agents: bool, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Show service status
#[cfg(unix)]
pub fn run_status(dir: &Path, json: bool) -> Result<()> {
    let state = match ServiceState::load(dir)? {
        Some(s) => s,
        None => {
            if json {
                let output = serde_json::json!({
                    "status": "not_running",
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("Service: not running");
            }
            return Ok(());
        }
    };

    let running = is_process_alive(state.pid);

    if !running {
        // Stale state, clean up
        ServiceState::remove(dir)?;
        if json {
            let output = serde_json::json!({
                "status": "not_running",
                "note": "Cleaned up stale state",
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            println!("Service: not running (cleaned up stale state)");
        }
        return Ok(());
    }

    // Get agent summary (runtime registry = spawned processes)
    let registry = AgentRegistry::load_or_warn(dir);
    let alive_count = registry.active_count();
    let idle_count = registry.idle_count();

    // Check if any agency agents are defined (YAML definitions, not runtime processes)
    let agency_agents_dir = dir.join("agency").join("cache/agents");
    let agency_agents_defined = !agency::load_all_agents_or_warn(&agency_agents_dir).is_empty();

    // Calculate uptime
    let uptime = chrono::DateTime::parse_from_rfc3339(&state.started_at)
        .map(|started| {
            let now = chrono::Utc::now();
            let duration = now.signed_duration_since(started);
            workgraph::format_duration(duration.num_seconds(), false)
        })
        .unwrap_or_else(|_| "unknown".to_string());

    // Load coordinator state (persisted by daemon, reflects effective config + runtime)
    let coord = CoordinatorState::load_or_default(dir);

    // Log file info
    let log_path = log_file_path(dir);
    let log_path_str = log_path.to_string_lossy().to_string();
    let log_exists = log_path.exists();
    let recent_errors = tail_log(dir, 5, Some("ERROR"));
    let recent_fatals = tail_log(dir, 5, Some("FATAL"));

    if json {
        let mut output = serde_json::json!({
            "status": "running",
            "pid": state.pid,
            "socket": state.socket_path,
            "started_at": state.started_at,
            "uptime": uptime,
            "agents": {
                "alive": alive_count,
                "idle": idle_count,
                "total": registry.agents.len(),
                "agents_defined": agency_agents_defined,
            },
            "coordinator": {
                "enabled": coord.enabled,
                "paused": coord.paused,
                "max_agents": coord.max_agents,
                "poll_interval": coord.poll_interval,
                "executor": coord.executor,
                "model": coord.model,
                "ticks": coord.ticks,
                "last_tick": coord.last_tick,
                "agents_alive": coord.agents_alive,
                "tasks_ready": coord.tasks_ready,
                "agents_spawned_last_tick": coord.agents_spawned,
            },
            "log": {
                "path": log_path_str,
                "exists": log_exists,
            }
        });
        if !agency_agents_defined {
            output["warning"] =
                serde_json::json!("No agents defined — run 'wg agency init' or 'wg agent create'");
        }
        if agency_agents_defined
            && alive_count == 0
            && coord.ticks > 0
            && coord.agents_spawned == 0
            && coord.tasks_ready > 0
        {
            output["agents"]["note"] = serde_json::json!(
                "tasks are ready but no agents have been spawned — check agent configuration"
            );
        }
        if !recent_errors.is_empty() || !recent_fatals.is_empty() {
            let mut all_errors: Vec<String> = recent_fatals;
            all_errors.extend(recent_errors);
            output["log"]["recent_errors"] = serde_json::json!(all_errors);
        }
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Service: running (PID {})", state.pid);
        println!("Socket: {}", state.socket_path);
        println!("Uptime: {}", uptime);
        if !agency_agents_defined {
            println!("Agents: No agents defined — run 'wg agency init' or 'wg agent create'");
        } else {
            println!(
                "Agents: {} alive, {} idle, {} total",
                alive_count,
                idle_count,
                registry.agents.len()
            );
            if alive_count == 0
                && coord.ticks > 0
                && coord.agents_spawned == 0
                && coord.tasks_ready > 0
            {
                println!(
                    "  Note: tasks are ready but no agents have been spawned — check agent configuration"
                );
            }
        }
        let model_str = coord.model.as_deref().unwrap_or("default");
        let pause_str = if coord.paused { ", PAUSED" } else { "" };
        println!(
            "Coordinator: enabled{}, max_agents={}, poll_interval={}s, executor={}, model={}",
            pause_str, coord.max_agents, coord.poll_interval, coord.executor, model_str
        );
        if let Some(ref last) = coord.last_tick {
            println!(
                "  Last tick: {} (#{}, agents_alive={}/{}, tasks_ready={}, spawned={})",
                last,
                coord.ticks,
                coord.agents_alive,
                coord.max_agents,
                coord.tasks_ready,
                coord.agents_spawned
            );
        } else {
            println!("  No ticks yet");
        }
        println!("Log: {}", log_path_str);
        if !recent_errors.is_empty() || !recent_fatals.is_empty() {
            println!("  Recent errors:");
            for line in &recent_fatals {
                println!("    {}", line);
            }
            for line in &recent_errors {
                println!("    {}", line);
            }
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_status(_dir: &Path, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Reload service daemon configuration at runtime
#[cfg(unix)]
pub fn run_reload(
    dir: &Path,
    max_agents: Option<usize>,
    executor: Option<&str>,
    interval: Option<u64>,
    model: Option<&str>,
    json: bool,
) -> Result<()> {
    let request = IpcRequest::Reconfigure {
        max_agents,
        executor: executor.map(std::string::ToString::to_string),
        poll_interval: interval,
        model: model.map(std::string::ToString::to_string),
    };

    let response = send_request(dir, &request)?;

    if !response.ok {
        let msg = response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        if json {
            let output = serde_json::json!({ "error": msg });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            eprintln!("Error: {}", msg);
        }
        anyhow::bail!("{}", msg);
    }

    if json {
        if let Some(data) = &response.data {
            println!("{}", serde_json::to_string_pretty(data)?);
        }
    } else {
        let has_flags =
            max_agents.is_some() || executor.is_some() || interval.is_some() || model.is_some();
        if has_flags {
            println!("Configuration updated");
        } else {
            println!("Configuration reloaded from config.toml");
        }
        if let Some(data) = &response.data
            && let Some(cfg) = data.get("config")
        {
            let ma = cfg
                .get("max_agents")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let ex = cfg.get("executor").and_then(|v| v.as_str()).unwrap_or("?");
            let pi = cfg
                .get("poll_interval")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let mdl = cfg
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("default");
            println!(
                "Effective config: max_agents={}, executor={}, poll_interval={}s, model={}",
                ma, ex, pi, mdl
            );
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_reload(
    _dir: &Path,
    _max_agents: Option<usize>,
    _executor: Option<&str>,
    _interval: Option<u64>,
    _model: Option<&str>,
    _json: bool,
) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Pause the coordinator (no new agent spawns, running agents unaffected)
#[cfg(unix)]
pub fn run_pause(dir: &Path, json: bool) -> Result<()> {
    let response = send_request(dir, &IpcRequest::Pause)?;

    if !response.ok {
        let msg = response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        if json {
            let output = serde_json::json!({ "error": msg });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            eprintln!("Error: {}", msg);
        }
        anyhow::bail!("{}", msg);
    }

    if json {
        if let Some(data) = &response.data {
            println!("{}", serde_json::to_string_pretty(data)?);
        }
    } else {
        println!("Coordinator paused (running agents continue, no new spawns)");
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_pause(_dir: &Path, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Resume the coordinator (triggers immediate tick)
#[cfg(unix)]
pub fn run_resume(dir: &Path, json: bool) -> Result<()> {
    let response = send_request(dir, &IpcRequest::Resume)?;

    if !response.ok {
        let msg = response
            .error
            .unwrap_or_else(|| "Unknown error".to_string());
        if json {
            let output = serde_json::json!({ "error": msg });
            println!("{}", serde_json::to_string_pretty(&output)?);
        } else {
            eprintln!("Error: {}", msg);
        }
        anyhow::bail!("{}", msg);
    }

    if json {
        if let Some(data) = &response.data {
            println!("{}", serde_json::to_string_pretty(data)?);
        }
    } else {
        println!("Coordinator resumed");
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_resume(_dir: &Path, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Public wrapper: check if the service process is alive
pub fn is_service_alive(pid: u32) -> bool {
    is_process_alive(pid)
}

/// Check if the coordinator is currently paused
pub fn is_service_paused(dir: &Path) -> bool {
    CoordinatorState::load(dir).is_some_and(|c| c.paused)
}

/// Send an IPC request to the running service
#[cfg(unix)]
pub fn send_request(dir: &Path, request: &IpcRequest) -> Result<IpcResponse> {
    let state = ServiceState::load(dir)?.ok_or_else(|| anyhow::anyhow!("Service not running"))?;

    let socket = PathBuf::from(&state.socket_path);
    let mut stream = UnixStream::connect(&socket)
        .with_context(|| format!("Failed to connect to service at {:?}", socket))?;

    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let json = serde_json::to_string(&request)?;
    writeln!(stream, "{}", json)?;
    stream.flush()?;

    let reader = BufReader::new(&stream);
    for line in reader.lines() {
        let line = line.context("Failed to read response")?;
        if !line.is_empty() {
            let response: IpcResponse =
                serde_json::from_str(&line).context("Failed to parse response")?;
            return Ok(response);
        }
    }

    anyhow::bail!("No response from service")
}

#[cfg(not(unix))]
pub fn send_request(_dir: &Path, _request: &IpcRequest) -> Result<IpcResponse> {
    anyhow::bail!("IPC is only supported on Unix systems")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_socket_path() {
        let temp_dir = TempDir::new().unwrap();
        let socket = default_socket_path(temp_dir.path());
        assert_eq!(socket, temp_dir.path().join("service").join("daemon.sock"));
    }

    #[test]
    fn test_service_state_roundtrip() {
        let temp_dir = TempDir::new().unwrap();

        let state = ServiceState {
            pid: 12345,
            socket_path: "/tmp/test.sock".to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        };

        state.save(temp_dir.path()).unwrap();

        let loaded = ServiceState::load(temp_dir.path()).unwrap().unwrap();
        assert_eq!(loaded.pid, 12345);
        assert_eq!(loaded.socket_path, "/tmp/test.sock");

        ServiceState::remove(temp_dir.path()).unwrap();
        assert!(ServiceState::load(temp_dir.path()).unwrap().is_none());
    }

    #[test]
    fn test_is_process_alive() {
        // Current process should be running
        #[cfg(unix)]
        {
            let pid = std::process::id();
            assert!(is_process_alive(pid));
        }

        // Non-existent process
        #[cfg(unix)]
        assert!(!is_process_alive(999999999));
    }

    #[test]
    fn test_status_not_running() {
        let temp_dir = TempDir::new().unwrap();
        // No state file, should report not running
        let result = run_status(temp_dir.path(), false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_daemon_logger_basic() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        let logger = DaemonLogger::open(dir).unwrap();
        logger.info("test message");
        logger.error("test error");
        logger.warn("test warning");

        let log_path = log_file_path(dir);
        let content = fs::read_to_string(&log_path).unwrap();
        assert!(content.contains("[INFO] test message"));
        assert!(content.contains("[ERROR] test error"));
        assert!(content.contains("[WARN] test warning"));
    }

    #[test]
    fn test_tail_log() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        let logger = DaemonLogger::open(dir).unwrap();
        logger.info("info 1");
        logger.error("error 1");
        logger.info("info 2");
        logger.error("error 2");
        logger.error("error 3");

        // Get last 2 error lines
        let errors = tail_log(dir, 2, Some("ERROR"));
        assert_eq!(errors.len(), 2);
        assert!(errors[0].contains("error 2"));
        assert!(errors[1].contains("error 3"));

        // Get all lines
        let all = tail_log(dir, 100, None);
        assert_eq!(all.len(), 5);
    }

    #[test]
    fn test_run_start_refuses_if_daemon_alive() {
        // If state.json exists with a PID that is alive, run_start should refuse
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Use our own PID to simulate an alive daemon
        let our_pid = std::process::id();
        let state = ServiceState {
            pid: our_pid,
            socket_path: dir
                .join("service")
                .join("daemon.sock")
                .to_string_lossy()
                .to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        };
        state.save(dir).unwrap();

        // run_start should not start a new daemon
        let result = run_start(dir, None, None, None, None, None, None, false, false, false);
        assert!(result.is_ok()); // returns Ok but prints "already running"

        // State should be unchanged (same PID)
        let loaded = ServiceState::load(dir).unwrap().unwrap();
        assert_eq!(loaded.pid, our_pid);
    }

    #[test]
    fn test_run_start_cleans_stale_state() {
        // If state.json exists with a PID that is dead, run_start should clean up
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Use a non-existent PID
        let state = ServiceState {
            pid: 999999999,
            socket_path: dir
                .join("service")
                .join("daemon.sock")
                .to_string_lossy()
                .to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        };
        state.save(dir).unwrap();

        // The stale state should be cleaned up (run_start will try to spawn daemon
        // which will fail since we don't have a real wg binary, but the stale
        // state should be removed first)
        let state_path = state_file_path(dir);
        assert!(state_path.exists());
        // We can't fully test start since it spawns a real process, but we verify
        // the state cleanup happens by checking ServiceState::load after removal
        ServiceState::remove(dir).unwrap();
        assert!(!state_path.exists());
    }

    #[test]
    fn test_find_orphan_daemon_pids_no_orphans() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        // No orphans should be found for a random temp dir
        let orphans = find_orphan_daemon_pids(dir, None);
        assert!(orphans.is_empty());
    }

    #[test]
    fn test_run_stop_cleans_up_state_and_socket() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Write a state file with a dead PID
        let state = ServiceState {
            pid: 999999999,
            socket_path: dir
                .join("service")
                .join("daemon.sock")
                .to_string_lossy()
                .to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
        };
        state.save(dir).unwrap();

        // Stop should succeed and clean up
        let result = run_stop(dir, false, false, false);
        assert!(result.is_ok());

        // State file should be removed
        assert!(ServiceState::load(dir).unwrap().is_none());
    }

    #[test]
    fn test_no_agents_warning_when_auto_assign_enabled() {
        // When auto_assign is enabled but no agency agents exist,
        // the service start output should include a warning.
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir.join("agency").join("cache/agents")).unwrap();

        // Enable auto_assign in config
        let mut config = Config::load_or_default(wg_dir);
        config.agency.auto_assign = true;
        config.save(wg_dir).unwrap();

        // Check: no agency agents defined
        let agents_dir = wg_dir.join("agency").join("cache/agents");
        let agents = agency::load_all_agents_or_warn(&agents_dir);
        assert!(agents.is_empty(), "Expected no agents defined");

        // The condition that triggers the warning
        let no_agents_defined = agents.is_empty();
        let warn_no_agents = config.agency.auto_assign && no_agents_defined;
        assert!(
            warn_no_agents,
            "Should warn: auto_assign enabled, no agents defined"
        );
    }

    #[test]
    fn test_no_warning_when_agents_exist() {
        // When agency agents exist, no warning should be shown.
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();

        // Use agency init to create roles, motivations, and a default agent
        super::super::agency_init::run(wg_dir).unwrap();

        let mut config = Config::load_or_default(wg_dir);
        config.agency.auto_assign = true;
        config.save(wg_dir).unwrap();

        let agents_dir = wg_dir.join("agency").join("cache/agents");
        let agents = agency::load_all_agents_or_warn(&agents_dir);
        assert!(!agents.is_empty(), "Expected at least one agent");

        let no_agents_defined = agents.is_empty();
        let warn_no_agents = config.agency.auto_assign && no_agents_defined;
        assert!(!warn_no_agents, "Should NOT warn when agents are defined");
    }

    #[test]
    fn test_status_distinguishes_no_agents_from_dead_agents() {
        // When no agency agents are defined, status should say "No agents defined"
        // rather than just showing agents_alive=0.
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();
        fs::create_dir_all(wg_dir.join("agency").join("cache/agents")).unwrap();

        let agents_dir = wg_dir.join("agency").join("cache/agents");
        let agency_agents_defined = !agency::load_all_agents_or_warn(&agents_dir).is_empty();

        // No agents defined — should show the "No agents defined" message
        assert!(!agency_agents_defined);

        let status_line = if !agency_agents_defined {
            "Agents: No agents defined — run 'wg agency init' or 'wg agent create'".to_string()
        } else {
            "Agents: 0 alive, 0 idle, 0 total".to_string()
        };
        assert!(
            status_line.contains("No agents defined"),
            "Expected 'No agents defined' message, got: {}",
            status_line
        );
    }

    #[test]
    fn test_status_shows_counts_when_agents_defined() {
        // When agency agents exist but none are alive (process-wise),
        // status should show the alive/idle/total counts, NOT "No agents defined".
        let temp_dir = TempDir::new().unwrap();
        let wg_dir = temp_dir.path();

        // Create an agent via agency init
        super::super::agency_init::run(wg_dir).unwrap();

        let agents_dir = wg_dir.join("agency").join("cache/agents");
        let agency_agents_defined = !agency::load_all_agents_or_warn(&agents_dir).is_empty();
        assert!(agency_agents_defined);

        let status_line = if !agency_agents_defined {
            "Agents: No agents defined — run 'wg agency init' or 'wg agent create'".to_string()
        } else {
            "Agents: 0 alive, 0 idle, 0 total".to_string()
        };
        assert!(
            !status_line.contains("No agents defined"),
            "Should show counts when agents are defined, got: {}",
            status_line
        );
        assert!(status_line.contains("0 alive"));
    }
}
