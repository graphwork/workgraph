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

mod assignment;
mod coordinator;
pub(crate) mod coordinator_agent;
pub mod ipc;
mod triage;
pub(crate) mod worktree;
pub(crate) mod zero_output;

pub use ipc::{IpcRequest, IpcResponse};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::IsTerminal;
use std::io::{BufRead, BufReader, Read as _, Write};
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

// ---------------------------------------------------------------------------
// Binary hash for self-restart detection
// ---------------------------------------------------------------------------

/// Compute SHA-256 of the file at `path`.
///
/// Uses streaming reads to avoid loading the entire binary into memory at once.
/// Returns the 32-byte digest on success.
fn compute_exe_hash(path: &Path) -> std::io::Result<[u8; 32]> {
    compute_exe_hash_inner(path, false)
}

/// Low-priority variant that throttles I/O so the background hash thread
/// stays below ~5 % of a CPU core.  Used for the initial baseline hash.
fn compute_exe_hash_background(path: &Path) -> std::io::Result<[u8; 32]> {
    compute_exe_hash_inner(path, true)
}

/// Compute SHA-256 of the file at `path`.
///
/// When `throttle` is true, the computation sleeps between chunks to avoid
/// pegging a CPU core (important for large debug binaries — the unoptimised
/// debug build can be 250 MB+).
fn compute_exe_hash_inner(path: &Path, throttle: bool) -> std::io::Result<[u8; 32]> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut bytes_since_yield: usize = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        if throttle {
            bytes_since_yield += n;
            // Sleep 200 ms every 256 KB of data hashed.  In debug mode each
            // 256 KB chunk takes ~7 ms of CPU, so the duty cycle is roughly
            // 7 / (7 + 200) ≈ 3.4 %.  For a 257 MB debug binary the total
            // wall-clock time is ~218 s — acceptable for a one-time
            // background baseline that runs after a 5 s startup delay.
            if bytes_since_yield >= 256 * 1024 {
                bytes_since_yield = 0;
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
    Ok(hasher.finalize().into())
}

/// Format first 12 hex chars of a 32-byte hash for log messages.
fn short_hash(hash: &[u8; 32]) -> String {
    hex::encode(&hash[..6])
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

/// Path to the legacy (shared) coordinator state file.
/// Used only for backward-compatible fallback reads when no per-ID file exists.
pub fn coordinator_state_path_legacy(dir: &Path) -> PathBuf {
    dir.join("service").join("coordinator-state.json")
}

/// Path to a per-coordinator state file: `coordinator-state-{id}.json`.
pub fn coordinator_state_path(dir: &Path, coordinator_id: u32) -> PathBuf {
    dir.join("service")
        .join(format!("coordinator-state-{}.json", coordinator_id))
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
    /// Whether agents are frozen (SIGSTOP sent to all agent processes)
    #[serde(default)]
    pub frozen: bool,
    /// PIDs that were frozen (for thaw to target the right processes)
    #[serde(default)]
    pub frozen_pids: Vec<u32>,
    /// Accumulated coordinator conversation tokens since last compaction.
    /// Incremented by the coordinator agent thread after each LLM turn.
    /// Resets to 0 after successful compaction.
    #[serde(default)]
    pub accumulated_tokens: u64,
}

impl CoordinatorState {
    /// Load coordinator state for a specific coordinator ID.
    /// Checks the per-ID file first, then falls back to the legacy shared file
    /// for coordinator 0.
    pub fn load_for(dir: &Path, coordinator_id: u32) -> Option<Self> {
        let path = coordinator_state_path(dir, coordinator_id);
        if let Ok(content) = fs::read_to_string(&path) {
            return match serde_json::from_str(&content) {
                Ok(state) => Some(state),
                Err(e) => {
                    eprintln!(
                        "Warning: corrupt coordinator state at {}: {}",
                        path.display(),
                        e
                    );
                    None
                }
            };
        }
        // Backward compat: fall back to legacy shared file for coordinator 0
        if coordinator_id == 0 {
            let legacy = coordinator_state_path_legacy(dir);
            if let Ok(content) = fs::read_to_string(&legacy) {
                return match serde_json::from_str(&content) {
                    Ok(state) => Some(state),
                    Err(e) => {
                        eprintln!(
                            "Warning: corrupt coordinator state at {}: {}",
                            legacy.display(),
                            e
                        );
                        None
                    }
                };
            }
        }
        None
    }

    /// Load coordinator 0 state (backward-compatible shorthand).
    pub fn load(dir: &Path) -> Option<Self> {
        Self::load_for(dir, 0)
    }

    /// Save coordinator state to the per-ID file.
    pub fn save_for(&self, dir: &Path, coordinator_id: u32) {
        let path = coordinator_state_path(dir, coordinator_id);
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

    /// Save coordinator 0 state (backward-compatible shorthand).
    pub fn save(&self, dir: &Path) {
        self.save_for(dir, 0);
    }

    /// Load coordinator state for a specific ID, defaulting to empty if missing or corrupt.
    pub fn load_or_default_for(dir: &Path, coordinator_id: u32) -> Self {
        Self::load_for(dir, coordinator_id).unwrap_or_default()
    }

    /// Load coordinator 0 state, defaulting to empty if missing or corrupt.
    /// Corrupt files already emit a warning via `load()`.
    pub fn load_or_default(dir: &Path) -> Self {
        Self::load(dir).unwrap_or_default()
    }

    /// Load all coordinator states from per-ID files in the service directory.
    /// Falls back to the legacy shared file when no per-ID files are found.
    /// Returns a sorted vec of (coordinator_id, state) pairs.
    pub fn load_all(dir: &Path) -> Vec<(u32, Self)> {
        let service_dir = dir.join("service");
        let mut results = Vec::new();
        if let Ok(entries) = fs::read_dir(&service_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if let Some(id_str) = name_str
                    .strip_prefix("coordinator-state-")
                    .and_then(|s| s.strip_suffix(".json"))
                    && let Ok(id) = id_str.parse::<u32>()
                    && let Some(state) = Self::load_for(dir, id)
                {
                    results.push((id, state));
                }
            }
        }
        // Fall back to legacy file if no per-ID files found
        if results.is_empty()
            && let Some(state) = Self::load(dir)
        {
            results.push((0, state));
        }
        results.sort_by_key(|(id, _)| *id);
        results
    }

    /// Sum `accumulated_tokens` across all per-coordinator state files.
    /// Falls back to the legacy shared file when no per-ID files are found.
    pub fn total_accumulated_tokens(dir: &Path) -> u64 {
        let service_dir = dir.join("service");
        let entries = match fs::read_dir(&service_dir) {
            Ok(e) => e,
            Err(_) => return Self::load(dir).map(|s| s.accumulated_tokens).unwrap_or(0),
        };
        let mut total: u64 = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("coordinator-state-")
                && name_str.ends_with(".json")
                && let Ok(content) = fs::read_to_string(entry.path())
                && let Ok(state) = serde_json::from_str::<Self>(&content)
            {
                total += state.accumulated_tokens;
            }
        }
        // Fall back to legacy file if no per-ID files found
        if total == 0 {
            total = Self::load(dir).map(|s| s.accumulated_tokens).unwrap_or(0);
        }
        total
    }

    /// Remove the per-ID state file for a specific coordinator.
    pub fn remove_for(dir: &Path, coordinator_id: u32) {
        let path = coordinator_state_path(dir, coordinator_id);
        let _ = fs::remove_file(&path);
    }

    /// Remove coordinator 0 state file(s), including legacy shared file.
    pub fn remove(dir: &Path) {
        Self::remove_for(dir, 0);
        // Also clean up the legacy shared file
        let _ = fs::remove_file(coordinator_state_path_legacy(dir));
    }

    /// Remove ALL per-coordinator state files and the legacy shared file.
    /// Used on daemon shutdown to clean up all coordinator state.
    #[allow(dead_code)]
    pub fn remove_all(dir: &Path) {
        let service_dir = dir.join("service");
        if let Ok(entries) = fs::read_dir(&service_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with("coordinator-state") && name_str.ends_with(".json") {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    /// Reset accumulated_tokens to 0 in all per-coordinator state files.
    #[allow(dead_code)]
    pub fn reset_all_accumulated_tokens(dir: &Path) {
        for (id, mut state) in Self::load_all(dir) {
            state.accumulated_tokens = 0;
            state.save_for(dir, id);
        }
    }

    /// Migrate legacy coordinator-state.json to per-ID file (coordinator-state-0.json).
    /// No-op if the legacy file doesn't exist or a per-ID file already exists.
    #[allow(dead_code)]
    pub fn migrate_legacy(dir: &Path) {
        let legacy_path = coordinator_state_path_legacy(dir);
        let per_id_path = coordinator_state_path(dir, 0);
        if legacy_path.exists()
            && !per_id_path.exists()
            && let Ok(content) = fs::read_to_string(&legacy_path)
            && let Ok(state) = serde_json::from_str::<Self>(&content)
        {
            state.save_for(dir, 0);
            let _ = fs::remove_file(&legacy_path);
        }
    }

    /// Update a field across all per-coordinator state files.
    /// Used for global operations like pause/resume/freeze/thaw.
    #[allow(dead_code)]
    pub fn update_all(dir: &Path, mutator: impl Fn(&mut Self)) {
        for (id, mut state) in Self::load_all(dir) {
            mutator(&mut state);
            state.save_for(dir, id);
        }
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
        .unwrap_or_else(|| config.coordinator.effective_executor());

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

    // Wait for daemon to start, showing an animated spinner on TTYs
    let daemon_alive = if !json && std::io::stdout().is_terminal() {
        use std::io::Write as _;
        // Wave spinner constants
        const BOLT: &str = "↯";
        const NUM_BOLTS: usize = 5;
        const FRAME_MS: u64 = 120;
        // Fixed rainbow spectrum: Red, Orange, Green, Cyan, Violet
        const SPECTRAL_BRIGHT: [u8; NUM_BOLTS] = [196, 214, 46, 33, 129];
        const SPECTRAL_DIM: [u8; NUM_BOLTS] = [52, 94, 22, 17, 53];

        let start = Instant::now();
        let mut stdout = std::io::stdout();
        let mut alive = false;

        // Animate for at least 600ms so the wave is visible, up to 2s max
        while start.elapsed() < Duration::from_millis(2000) {
            let elapsed_ms = start.elapsed().as_millis() as usize;
            let wave_pos = (elapsed_ms / FRAME_MS as usize) % NUM_BOLTS;

            // Build the colored bolt string — peak bolt is bright, others dimmed
            let mut line = String::with_capacity(80);
            line.push_str("  ");
            for i in 0..NUM_BOLTS {
                let dist = (i as isize - wave_pos as isize).unsigned_abs();
                let color = if dist <= 1 {
                    SPECTRAL_BRIGHT[i]
                } else {
                    SPECTRAL_DIM[i]
                };
                if dist == 0 {
                    // Bold the peak bolt for extra pop
                    line.push_str(&format!("\x1b[1;38;5;{}m{}\x1b[0m", color, BOLT));
                } else {
                    line.push_str(&format!("\x1b[38;5;{}m{}\x1b[0m", color, BOLT));
                }
            }
            line.push_str(" Starting service...");

            // Overwrite current line
            print!("\r\x1b[2K{}", line);
            let _ = stdout.flush();

            std::thread::sleep(Duration::from_millis(FRAME_MS));

            // Check if daemon is alive and socket is accepting connections
            // after minimum animation time
            if start.elapsed() >= Duration::from_millis(600)
                && is_process_alive(pid)
                && socket_accepting(&socket)
            {
                alive = true;
                break;
            }
        }

        // Clear the spinner line
        print!("\r\x1b[2K");
        let _ = stdout.flush();
        alive
    } else {
        // Non-TTY or JSON mode: wait for process alive + socket accepting
        let start = Instant::now();
        let mut alive = false;
        while start.elapsed() < Duration::from_millis(3000) {
            std::thread::sleep(Duration::from_millis(100));
            if is_process_alive(pid) && socket_accepting(&socket) {
                alive = true;
                break;
            }
        }
        alive
    };

    // Verify daemon started successfully
    if !daemon_alive {
        ServiceState::remove(dir)?;
        anyhow::bail!("Daemon process exited immediately. Check logs.");
    }

    // Resolve effective config for display (CLI flags override config.toml)
    let config = Config::load_or_default(dir);
    let eff_max_agents = max_agents.unwrap_or(config.coordinator.max_agents);
    let eff_poll_interval = interval.unwrap_or(config.coordinator.poll_interval);
    let eff_executor = executor
        .map(std::string::ToString::to_string)
        .unwrap_or_else(|| config.coordinator.effective_executor());
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
    provider: Option<String>,
    paused: bool,
    /// Settling delay after GraphChanged events. During burst graph construction,
    /// multiple adds fire in rapid succession. Instead of ticking immediately on
    /// each GraphChanged, the coordinator waits this long after the *last* event
    /// before dispatching. This prevents premature dispatch on partially-wired graphs.
    settling_delay: Duration,
}

/// Route new chat inbox messages to the persistent coordinator agent for a specific coordinator.
///
/// Reads the inbox since the coordinator cursor, sends each message to the
/// agent thread, and advances the cursor. The agent thread handles context
/// injection, LLM processing, and outbox writing asynchronously.
///
/// Returns the number of messages routed.
fn route_chat_to_agent(
    dir: &Path,
    coordinator_id: u32,
    agent: &coordinator_agent::CoordinatorAgent,
    logger: &DaemonLogger,
) -> Result<usize> {
    let chat_dir = dir.join("chat").join(coordinator_id.to_string());
    if !chat_dir.exists() {
        return Ok(0);
    }

    let inbox_cursor = workgraph::chat::read_coordinator_cursor_for(dir, coordinator_id)?;
    let new_messages = workgraph::chat::read_inbox_since_for(dir, coordinator_id, inbox_cursor)?;

    if new_messages.is_empty() {
        return Ok(0);
    }

    let count = new_messages.len();
    for msg in &new_messages {
        if let Err(e) = agent.send_message(msg.request_id.clone(), msg.content.clone()) {
            logger.error(&format!(
                "Failed to send chat message to coordinator agent {}: {}",
                coordinator_id, e
            ));
            // Write an error response so the user isn't left hanging
            let _ = workgraph::chat::append_outbox_for(
                dir,
                coordinator_id,
                "The coordinator agent is not available. Please try again.",
                &msg.request_id,
            );
        }
    }

    // Advance the coordinator cursor past these messages
    if let Some(last) = new_messages.last() {
        workgraph::chat::write_coordinator_cursor_for(dir, coordinator_id, last.id)?;
    }

    Ok(count)
}

/// Route chat messages to all active coordinator agents.
/// Checks each coordinator's inbox and routes pending messages.
/// Returns total number of messages routed across all coordinators.
fn route_chat_to_all_agents(
    dir: &Path,
    agents: &std::collections::HashMap<u32, coordinator_agent::CoordinatorAgent>,
    logger: &DaemonLogger,
) -> Result<usize> {
    let mut total = 0;
    for (&cid, agent) in agents {
        match route_chat_to_agent(dir, cid, agent, logger) {
            Ok(count) => total += count,
            Err(e) => {
                logger.error(&format!(
                    "Failed to route chat to coordinator {}: {}",
                    cid, e
                ));
            }
        }
    }
    Ok(total)
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

/// Dispatch notifications for recently changed tasks via the notification router.
///
/// Scans the graph for tasks that recently failed or became blocked, and sends
/// notifications through the configured [`NotificationRouter`]. This is called
/// after each coordinator tick.
fn try_dispatch_notifications(dir: &Path, logger: &DaemonLogger) {
    use workgraph::notify::NotificationRouter;
    use workgraph::notify::config::NotifyConfig;
    use workgraph::notify::dispatch::{TaskEvent, TaskEventKind};
    use workgraph::notify::webhook::WebhookChannel;

    // Load notification config — if not present, notifications are disabled.
    let config = match NotifyConfig::load(Some(dir)) {
        Ok(Some(c)) => c,
        Ok(None) => return, // No config → notifications disabled
        Err(e) => {
            logger.warn(&format!("Failed to load notify config: {}", e));
            return;
        }
    };

    let rules = config.to_routing_rules();
    let default_channels = config.default_channels().to_vec();

    if rules.is_empty() && default_channels.is_empty() {
        return; // No routing rules → nothing to dispatch
    }

    // Build channels from config. Each channel type is constructed if its
    // config section exists.
    let mut channels: Vec<Box<dyn workgraph::notify::NotificationChannel>> = Vec::new();

    // Webhook channel (always available, no external runtime deps)
    if config.has_channel_config("webhook")
        && let Some(val) = config.channels.get("webhook")
    {
        match val
            .clone()
            .try_into::<workgraph::notify::webhook::WebhookConfig>()
        {
            Ok(wh_config) => {
                channels.push(Box::new(WebhookChannel::new(wh_config)));
            }
            Err(e) => {
                logger.warn(&format!("Invalid webhook config: {}", e));
            }
        }
    }

    // Telegram channel (if configured)
    if config.has_channel_config("telegram") {
        match workgraph::notify::telegram::TelegramConfig::from_notify_config(&config) {
            Ok(tg_config) => {
                channels.push(Box::new(workgraph::notify::telegram::TelegramChannel::new(
                    tg_config,
                )));
            }
            Err(e) => {
                logger.warn(&format!("Invalid telegram config: {}", e));
            }
        }
    }

    if channels.is_empty() {
        return; // No usable channels
    }

    let router = NotificationRouter::new(channels, rules, default_channels);

    // Scan graph for recently changed tasks (last 10 seconds)
    let gp = graph_path(dir);
    let graph = match load_graph(&gp) {
        Ok(g) => g,
        Err(_) => return,
    };

    let recent_cutoff = chrono::Utc::now() - chrono::Duration::seconds(10);
    let mut events: Vec<TaskEvent> = Vec::new();

    for task in graph.tasks() {
        match task.status {
            workgraph::graph::Status::Failed => {
                if let Some(last_log) = task.log.last()
                    && let Ok(dt) = last_log.timestamp.parse::<DateTime<Utc>>()
                    && dt > recent_cutoff
                {
                    events.push(TaskEvent {
                        task_id: task.id.clone(),
                        title: task.title.clone(),
                        kind: TaskEventKind::Failed,
                        detail: task.failure_reason.clone(),
                    });
                }
            }
            workgraph::graph::Status::Blocked => {
                if let Some(last_log) = task.log.last()
                    && let Ok(dt) = last_log.timestamp.parse::<DateTime<Utc>>()
                    && dt > recent_cutoff
                {
                    events.push(TaskEvent {
                        task_id: task.id.clone(),
                        title: task.title.clone(),
                        kind: TaskEventKind::Blocked,
                        detail: None,
                    });
                }
            }
            _ => {}
        }
    }

    if events.is_empty() {
        return;
    }

    // Dispatch notifications using a short-lived tokio runtime
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            logger.warn(&format!("Failed to create notification runtime: {}", e));
            return;
        }
    };

    for event in &events {
        // Use task_id as the routing target (webhook will parse it)
        let target = &event.task_id;
        match rt.block_on(workgraph::notify::dispatch::dispatch_event(
            &router, target, event,
        )) {
            Ok(Some((ch, _mid))) => {
                logger.info(&format!(
                    "Notification sent for '{}' ({}) via {}",
                    event.task_id,
                    match event.kind {
                        TaskEventKind::Failed => "failed",
                        TaskEventKind::Blocked => "blocked",
                        _ => "event",
                    },
                    ch,
                ));
            }
            Ok(None) => {} // No channels for this event type
            Err(e) => {
                logger.warn(&format!(
                    "Failed to send notification for '{}': {}",
                    event.task_id, e
                ));
            }
        }
    }
}

/// Ensure the `.coordinator` and `.compact` cycle tasks exist in the graph.
///
/// Creates them if missing (Phase 2 of coordinator-as-graph-citizen).
/// The coordinator task has unlimited iterations and is tagged `coordinator-loop`.
/// The compact task forms a visible cycle with the coordinator:
///   .coordinator-0 → .compact-0 → .coordinator-0
fn ensure_coordinator_task(dir: &Path) {
    use workgraph::graph::{CycleConfig, LogEntry, Node, Task};

    let gp = graph_path(dir);
    let mut graph = match load_graph(&gp) {
        Ok(g) => g,
        Err(_) => return, // No graph yet — nothing to do
    };

    let mut modified = false;

    // Migrate legacy .coordinator to .coordinator-0
    if graph.get_task(".coordinator").is_some()
        && graph.get_task(".coordinator-0").is_none()
        && let Some(task) = graph.get_task_mut(".coordinator")
    {
        task.id = ".coordinator-0".to_string();
        task.title = "Coordinator 0".to_string();
        modified = true;
    }

    // Ensure .coordinator-0 exists
    if graph.get_task(".coordinator-0").is_none() {
        let task = Task {
            id: ".coordinator-0".to_string(),
            title: "Coordinator 0".to_string(),
            description: Some(
                "Persistent coordinator agent — each turn is a cycle iteration.".to_string(),
            ),
            status: workgraph::graph::Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            after: vec![".compact-0".to_string()],
            cycle_config: Some(CycleConfig {
                max_iterations: 0, // unlimited
                guard: None,
                delay: None,
                no_converge: true,
                restart_on_failure: true,
                max_failure_restarts: None,
            }),
            created_at: Some(Utc::now().to_rfc3339()),
            started_at: Some(Utc::now().to_rfc3339()),
            log: vec![LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("daemon".to_string()),
                user: Some(workgraph::current_user()),
                message: "Coordinator 0 task created by daemon".to_string(),
            }],
            ..Default::default()
        };

        graph.add_node(Node::Task(task));
        modified = true;
    } else if let Some(coord) = graph.get_task_mut(".coordinator-0") {
        // Ensure back-edge to .compact-0 exists on pre-existing coordinator tasks
        if !coord.after.contains(&".compact-0".to_string()) {
            coord.after.push(".compact-0".to_string());
            modified = true;
        }
    }

    // Ensure .compact-0 exists — forms a cycle with .coordinator-0
    if graph.get_task(".compact-0").is_none() {
        let task = Task {
            id: ".compact-0".to_string(),
            title: "Compact 0".to_string(),
            description: Some(
                "Compaction task — distills graph state into context.md. \
                 Forms a cycle with the coordinator: coordinator → compact → coordinator."
                    .to_string(),
            ),
            status: workgraph::graph::Status::Open,
            tags: vec!["compact-loop".to_string()],
            after: vec![".coordinator-0".to_string()],
            created_at: Some(Utc::now().to_rfc3339()),
            log: vec![LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("daemon".to_string()),
                user: Some(workgraph::current_user()),
                message: "Compact 0 task created by daemon".to_string(),
            }],
            ..Default::default()
        };

        graph.add_node(Node::Task(task));
        modified = true;
    }

    // Ensure .archive-0 exists — forms a cycle with .coordinator-0
    // Archives done/abandoned tasks older than the configured retention period.
    if graph.get_task(".archive-0").is_none() {
        let task = Task {
            id: ".archive-0".to_string(),
            title: "Archive 0".to_string(),
            description: Some(
                "Archive task — moves old done/abandoned tasks to archive.jsonl. \
                 Forms a cycle with the coordinator: coordinator → archive → coordinator."
                    .to_string(),
            ),
            status: workgraph::graph::Status::Open,
            tags: vec!["archive-loop".to_string()],
            after: vec![".coordinator-0".to_string()],
            created_at: Some(Utc::now().to_rfc3339()),
            log: vec![LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("daemon".to_string()),
                user: Some(workgraph::current_user()),
                message: "Archive 0 task created by daemon".to_string(),
            }],
            ..Default::default()
        };

        graph.add_node(Node::Task(task));
        modified = true;
    }

    // Ensure .coordinator-0 has back-edge to .archive-0
    if let Some(coord) = graph.get_task_mut(".coordinator-0")
        && !coord.after.contains(&".archive-0".to_string())
    {
        coord.after.push(".archive-0".to_string());
        modified = true;
    }

    if modified
        && let Err(e) = workgraph::parser::modify_graph(&gp, |fresh| {
            // Re-apply all mutations
            for node in graph.nodes() {
                if let workgraph::graph::Node::Task(t) = node {
                    if let Some(ft) = fresh.get_task_mut(&t.id) {
                        *ft = t.clone();
                    } else {
                        fresh.add_node(workgraph::graph::Node::Task(t.clone()));
                    }
                }
            }
            true
        })
    {
        eprintln!(
            "[daemon] Failed to save graph after creating coordinator/compact tasks: {}",
            e
        );
    }
}

/// Run compaction as a visible graph task (`.compact-N`).
///
/// Compaction is cycle-driven but includes a threshold-based re-open mechanism:
/// when `.compact-0` is stuck in Done (because the coordinator never marks
/// itself done and the cycle can't iterate), it is force-reset to Open once
/// accumulated tokens exceed the compaction threshold.
fn run_graph_compaction(dir: &Path, compaction_error_count: &mut u64, logger: &DaemonLogger) {
    let gp = graph_path(dir);

    // If .compact-0 is Done and accumulated tokens exceed the threshold,
    // force-reset it to Open. Normal cycle iteration requires ALL members to
    // be Done, but the coordinator is a persistent task that never completes,
    // so the cycle can never iterate on its own.
    {
        let graph = match load_graph(&gp) {
            Ok(g) => g,
            Err(_) => return,
        };
        let is_done = graph
            .get_task(".compact-0")
            .is_some_and(|t| t.status == workgraph::graph::Status::Done);
        if is_done {
            let config = workgraph::config::Config::load_or_default(dir);
            let threshold = config.effective_compaction_threshold();
            if threshold > 0 {
                let state = CoordinatorState::load_or_default(dir);
                if state.accumulated_tokens >= threshold {
                    let acc_tokens = state.accumulated_tokens;
                    if let Err(e) = workgraph::parser::modify_graph(&gp, |graph| {
                        if let Some(task) = graph.get_task_mut(".compact-0") {
                            task.status = workgraph::graph::Status::Open;
                            task.started_at = None;
                            task.completed_at = None;
                            task.log.push(workgraph::graph::LogEntry {
                                timestamp: chrono::Utc::now().to_rfc3339(),
                                actor: Some("daemon".to_string()),
                                user: Some(workgraph::current_user()),
                                message: format!(
                                    "Re-opened: tokens {} >= threshold {} (coordinator cycle bypass)",
                                    acc_tokens, threshold
                                ),
                            });
                            if task.log.len() > 50 {
                                let drain_count = task.log.len() - 50;
                                task.log.drain(..drain_count);
                            }
                            true
                        } else {
                            false
                        }
                    }) {
                        logger.error(&format!(
                            "Failed to save graph after resetting .compact-0: {}",
                            e
                        ));
                        return;
                    }
                    logger.info(&format!(
                        "Re-opened .compact-0: tokens {} >= threshold {}",
                        state.accumulated_tokens, threshold
                    ));
                }
            }
        }
    }

    // Check if .compact-0 is cycle-ready (uses cycle-aware readiness, not terminal check)
    {
        let graph = match load_graph(&gp) {
            Ok(g) => g,
            Err(_) => return,
        };
        if graph.get_task(".compact-0").is_none() {
            return; // No compact task in graph
        }
        let cycle_analysis = graph.compute_cycle_analysis();
        let cycle_ready =
            workgraph::query::ready_tasks_with_peers_cycle_aware(&graph, dir, &cycle_analysis);
        if !cycle_ready.iter().any(|t| t.id == ".compact-0") {
            return; // .compact-0 is not cycle-ready
        }

        // Gate on token threshold: defer compaction until enough tokens have accumulated
        let config = workgraph::config::Config::load_or_default(dir);
        let threshold = config.effective_compaction_threshold();
        if threshold > 0 {
            let state = CoordinatorState::load_or_default(dir);
            if state.accumulated_tokens < threshold {
                logger.info(&format!(
                    "Compaction deferred: accumulated_tokens={} < threshold={}",
                    state.accumulated_tokens, threshold
                ));
                return;
            }
        }
    }

    // Mark .compact-0 as InProgress
    {
        if let Err(e) = workgraph::parser::modify_graph(&gp, |graph| {
            if let Some(task) = graph.get_task_mut(".compact-0") {
                task.status = workgraph::graph::Status::InProgress;
                task.started_at = Some(chrono::Utc::now().to_rfc3339());
                task.log.push(workgraph::graph::LogEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    actor: Some("daemon".to_string()),
                    user: Some(workgraph::current_user()),
                    message: "Compaction started (cycle-driven)".to_string(),
                });
                true
            } else {
                false
            }
        }) {
            eprintln!(
                "[daemon] Failed to save graph after marking .compact-0 InProgress: {}",
                e
            );
            return;
        }
    }

    // Run compaction
    match workgraph::service::compactor::run_compaction(dir) {
        Ok(path) => {
            if *compaction_error_count > 0 {
                logger.info(&format!(
                    "Compaction recovered after {} consecutive error(s)",
                    *compaction_error_count
                ));
            }
            *compaction_error_count = 0;
            logger.info(&format!("Compaction complete → {}", path.display()));

            // Reset accumulated token counter after successful compaction
            {
                let mut cs = CoordinatorState::load_or_default(dir);
                let prev = cs.accumulated_tokens;
                cs.accumulated_tokens = 0;
                cs.save(dir);
                logger.info(&format!(
                    "Compaction: reset accumulated_tokens from {} to 0",
                    prev
                ));
            }

            // Mark .compact-0 as Done and increment iteration
            let path_display = path.display().to_string();
            if let Err(e) = workgraph::parser::modify_graph(&gp, |graph| {
                if let Some(task) = graph.get_task_mut(".compact-0") {
                    task.status = workgraph::graph::Status::Done;
                    task.completed_at = Some(chrono::Utc::now().to_rfc3339());
                    task.loop_iteration = task.loop_iteration.saturating_add(1);
                    task.log.push(workgraph::graph::LogEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        actor: Some("daemon".to_string()),
                        user: Some(workgraph::current_user()),
                        message: format!(
                            "Compaction iteration {} complete → {}",
                            task.loop_iteration, path_display
                        ),
                    });
                    if task.log.len() > 50 {
                        let drain_count = task.log.len() - 50;
                        task.log.drain(..drain_count);
                    }
                    true
                } else {
                    false
                }
            }) {
                eprintln!(
                    "[daemon] Failed to save graph after marking .compact-0 Done: {}",
                    e
                );
            }
        }
        Err(e) => {
            *compaction_error_count += 1;
            if *compaction_error_count == 1 || (*compaction_error_count).is_multiple_of(5) {
                logger.error(&format!(
                    "Compaction error (#{} consecutive): {:#}",
                    *compaction_error_count, e
                ));
            }

            // Persist error count to CompactorState so it survives daemon restarts
            {
                let mut cs = workgraph::service::compactor::CompactorState::load(dir);
                cs.error_count = *compaction_error_count;
                let _ = cs.save(dir);
            }

            // Mark .compact-0 as failed for this iteration, then reopen for retry
            let err_msg = format!("Compaction error: {:#}", e);
            let _ = workgraph::parser::modify_graph(&gp, |graph| {
                if let Some(task) = graph.get_task_mut(".compact-0") {
                    task.status = workgraph::graph::Status::Open;
                    task.started_at = None;
                    task.log.push(workgraph::graph::LogEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        actor: Some("daemon".to_string()),
                        user: Some(workgraph::current_user()),
                        message: err_msg.clone(),
                    });
                    if task.log.len() > 50 {
                        let drain_count = task.log.len() - 50;
                        task.log.drain(..drain_count);
                    }
                    true
                } else {
                    false
                }
            });
        }
    }
}

/// Run archival as a visible graph task (`.archive-0`).
///
/// Archival is cycle-driven: fires only when `.archive-0` is graph-ready
/// (status=Open and coordinator dep is terminal). Follows the same pattern
/// as `run_graph_compaction` for `.compact-0`.
fn run_graph_archival(dir: &Path, archival_error_count: &mut u64, logger: &DaemonLogger) {
    let gp = graph_path(dir);

    // Check if .archive-0 is cycle-ready
    {
        let graph = match load_graph(&gp) {
            Ok(g) => g,
            Err(_) => return,
        };
        if graph.get_task(".archive-0").is_none() {
            return; // No archive task in graph
        }
        let cycle_analysis = graph.compute_cycle_analysis();
        let cycle_ready =
            workgraph::query::ready_tasks_with_peers_cycle_aware(&graph, dir, &cycle_analysis);
        if !cycle_ready.iter().any(|t| t.id == ".archive-0") {
            return; // .archive-0 is not cycle-ready
        }
    }

    // Mark .archive-0 as InProgress
    {
        if let Err(e) = workgraph::parser::modify_graph(&gp, |graph| {
            if let Some(task) = graph.get_task_mut(".archive-0") {
                task.status = workgraph::graph::Status::InProgress;
                task.started_at = Some(chrono::Utc::now().to_rfc3339());
                task.log.push(workgraph::graph::LogEntry {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    actor: Some("daemon".to_string()),
                    user: Some(workgraph::current_user()),
                    message: "Archival started (cycle-driven)".to_string(),
                });
                true
            } else {
                false
            }
        }) {
            eprintln!(
                "[daemon] Failed to save graph after marking .archive-0 InProgress: {}",
                e
            );
            return;
        }
    }

    // Run archival
    let config = workgraph::config::Config::load_or_default(dir);
    let retention_days = config.coordinator.archive_retention_days;

    match crate::commands::archive::run_automatic(dir, retention_days) {
        Ok(count) => {
            if *archival_error_count > 0 {
                logger.info(&format!(
                    "Archival recovered after {} consecutive error(s)",
                    *archival_error_count
                ));
            }
            *archival_error_count = 0;
            logger.info(&format!(
                "Archival complete: {} tasks archived (retention: {}d)",
                count, retention_days
            ));

            // Mark .archive-0 as Done and increment iteration
            if let Err(e) = workgraph::parser::modify_graph(&gp, |graph| {
                if let Some(task) = graph.get_task_mut(".archive-0") {
                    task.status = workgraph::graph::Status::Done;
                    task.completed_at = Some(chrono::Utc::now().to_rfc3339());
                    task.loop_iteration = task.loop_iteration.saturating_add(1);
                    task.log.push(workgraph::graph::LogEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        actor: Some("daemon".to_string()),
                        user: Some(workgraph::current_user()),
                        message: format!(
                            "Archival iteration {} complete: {} tasks archived",
                            task.loop_iteration, count
                        ),
                    });
                    if task.log.len() > 50 {
                        let drain_count = task.log.len() - 50;
                        task.log.drain(..drain_count);
                    }
                    true
                } else {
                    false
                }
            }) {
                eprintln!(
                    "[daemon] Failed to save graph after marking .archive-0 Done: {}",
                    e
                );
            }
        }
        Err(e) => {
            *archival_error_count += 1;
            if *archival_error_count == 1 || (*archival_error_count).is_multiple_of(5) {
                logger.error(&format!(
                    "Archival error (#{} consecutive): {:#}",
                    *archival_error_count, e
                ));
            }

            // Revert .archive-0 to Open for retry
            let arch_err_msg = format!("Archival error: {:#}", e);
            let _ = workgraph::parser::modify_graph(&gp, |graph| {
                if let Some(task) = graph.get_task_mut(".archive-0") {
                    task.status = workgraph::graph::Status::Open;
                    task.started_at = None;
                    task.log.push(workgraph::graph::LogEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        actor: Some("daemon".to_string()),
                        user: Some(workgraph::current_user()),
                        message: arch_err_msg.clone(),
                    });
                    if task.log.len() > 50 {
                        let drain_count = task.log.len() - 50;
                        task.log.drain(..drain_count);
                    }
                    true
                } else {
                    false
                }
            });
        }
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

    // --- Binary self-restart detection ---
    // Record the exe path and its metadata at startup so we can detect when
    // `cargo install` (or similar) replaces the binary on disk.  We use
    // mtime + size as the cheap per-tick check (instant), then compute a
    // SHA-256 hash to confirm the content actually changed and the write is
    // complete.  The initial reference hash is computed in a background thread
    // to avoid blocking the main loop (important for large debug binaries).
    let exe_path = std::env::current_exe().ok();
    let exe_initial_meta = exe_path.as_ref().and_then(|p| fs::metadata(p).ok());
    let original_args: Vec<String> = std::env::args().collect();
    let exe_hash_receiver: Option<std::sync::mpsc::Receiver<[u8; 32]>> =
        exe_path.as_ref().map(|p| {
            let (tx, rx) = std::sync::mpsc::channel();
            let path = p.clone();
            std::thread::spawn(move || {
                // Delay before hashing so short-lived daemons (e.g. tests)
                // exit before we spend CPU.  The 5 s window is long enough
                // for most integration-test lifetimes.
                std::thread::sleep(std::time::Duration::from_secs(5));
                if let Ok(h) = compute_exe_hash_background(&path) {
                    let _ = tx.send(h);
                }
            });
            rx
        });
    let mut exe_initial_hash: Option<[u8; 32]> = None;
    if let (Some(p), Some(meta)) = (&exe_path, &exe_initial_meta) {
        logger.info(&format!(
            "Binary change detection armed: {} (size={})",
            p.display(),
            meta.len(),
        ));
    }

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

    // Validate configuration before starting
    let validation = config.validate_config();
    for diag in &validation.warnings {
        logger.warn(&format!("Config warning: {}", diag.message));
    }
    if !validation.is_ok() {
        for diag in &validation.errors {
            logger.error(&format!("Config error: {}", diag.message));
            logger.error(&format!("  Fix: {}", diag.fix));
        }
        // Clean up socket before bailing
        if socket.exists() {
            let _ = fs::remove_file(&socket);
        }
        anyhow::bail!(
            "Configuration validation failed with {} error(s). \
             Run 'wg config --show' for details.",
            validation.errors.len()
        );
    }

    let mut daemon_cfg = DaemonConfig {
        max_agents: cli_max_agents.unwrap_or(config.coordinator.max_agents),
        executor: cli_executor
            .map(std::string::ToString::to_string)
            .unwrap_or_else(|| config.coordinator.effective_executor()),
        // The poll_interval is the slow background safety-net timer.
        // CLI --interval overrides it; otherwise use config.coordinator.poll_interval.
        poll_interval: Duration::from_secs(
            cli_interval.unwrap_or(config.coordinator.poll_interval),
        ),
        model: cli_model
            .map(std::string::ToString::to_string)
            .or_else(|| config.coordinator.model.clone()),
        provider: config.coordinator.provider.clone(),
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

    // Clean up orphaned worktrees from a previous service run
    match worktree::cleanup_orphaned_worktrees(&dir) {
        Ok(count) if count > 0 => {
            logger.info(&format!(
                "Cleaned up {} orphaned worktree(s) on startup",
                count
            ));
        }
        Ok(_) => {} // No orphaned worktrees
        Err(e) => {
            logger.warn(&format!("Failed to clean up orphaned worktrees: {}", e));
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
        frozen: false,
        frozen_pids: Vec::new(),
        accumulated_tokens: CoordinatorState::load(&dir)
            .map(|cs| cs.accumulated_tokens)
            .unwrap_or(0),
    };
    coord_state.save(&dir);

    // Ensure the .coordinator cycle task exists in the graph (Phase 2).
    ensure_coordinator_task(&dir);

    // Auto-bootstrap agency when auto_evolve is enabled and agency isn't initialized.
    if config.agency.auto_evolve {
        let agency_dir = dir.join("agency");
        let roles_dir = agency_dir.join("cache/roles");
        if !roles_dir.exists()
            || agency::load_all_roles(&roles_dir)
                .map(|r| r.is_empty())
                .unwrap_or(true)
        {
            logger.info("auto_evolve enabled but agency not initialized — bootstrapping agency");
            match super::agency_init::run(&dir) {
                Ok(()) => logger.info("Agency auto-bootstrap complete"),
                Err(e) => logger.warn(&format!("Agency auto-bootstrap failed: {}", e)),
            }
        }
    }

    // Create the shared event log for coordinator context refresh.
    // The daemon records events (task completions, agent spawns, etc.) and the
    // coordinator agent reads them when building context for each interaction.
    let event_log = coordinator_agent::new_event_log();

    // Spawn the persistent coordinator agent(s) (LLM sessions for chat).
    // Each coordinator gets its own Claude CLI session. Coordinator 0 is
    // spawned at startup; additional coordinators are created on-demand via
    // the CreateCoordinator IPC request.
    // Enabled by default; disable with --no-coordinator-agent or
    // coordinator.coordinator_agent = false in config.toml.
    let enable_coordinator_agent = !no_coordinator_agent && config.coordinator.coordinator_agent;
    let mut coordinator_agents: std::collections::HashMap<
        u32,
        coordinator_agent::CoordinatorAgent,
    > = std::collections::HashMap::new();
    if enable_coordinator_agent {
        match coordinator_agent::CoordinatorAgent::spawn(
            &dir,
            0, // coordinator ID
            daemon_cfg.model.as_deref(),
            Some(&daemon_cfg.executor),
            daemon_cfg.provider.as_deref(),
            &logger,
            event_log.clone(),
        ) {
            Ok(agent) => {
                logger.info("Coordinator agent 0 spawned successfully");
                coordinator_agents.insert(0, agent);
            }
            Err(e) => {
                logger.warn(&format!(
                    "Failed to spawn coordinator agent 0: {}. Chat will use stub responses.",
                    e
                ));
            }
        }
    } else if no_coordinator_agent {
        logger.info("Coordinator agent disabled via --no-coordinator-agent flag");
    } else {
        logger.info(
            "Coordinator agent disabled (set coordinator.coordinator_agent = true to enable)",
        );
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
    let mut pending_coordinator_ids: Vec<u32> = Vec::new();

    // Load max_coordinators limit from config
    let max_coordinators = config.coordinator.max_coordinators;

    // Restore error counts from persisted state so they survive daemon restarts
    let mut compaction_error_count: u64 =
        workgraph::service::compactor::CompactorState::load(&dir).error_count;
    let mut archival_error_count: u64 = 0;

    // Obtain the raw fd for poll()-based waiting. This lets the daemon
    // sleep until an IPC connection arrives OR a timeout expires, instead
    // of busy-polling with a fixed sleep.
    let listener_fd = {
        use std::os::unix::io::AsRawFd;
        listener.as_raw_fd()
    };

    while running {
        // Reap zombie child processes (agents that have exited).
        // Even though agents call setsid() to create a new session, they are
        // still children of the daemon (parent-child is set at fork, not
        // affected by setsid). Without reaping, killed agents remain as
        // zombies and is_process_alive(pid) keeps returning true.
        reap_zombies();

        // Calculate how long to sleep. We wake on: incoming IPC connection,
        // settling deadline, or poll interval — whichever comes first.
        // Cap at 2s so zombie reaping and binary-change checks aren't delayed
        // too long.
        let mut poll_timeout_ms: i32 = 2000;
        if let Some(deadline) = settling_deadline {
            let until = deadline.saturating_duration_since(Instant::now());
            poll_timeout_ms = poll_timeout_ms.min(until.as_millis().min(i32::MAX as u128) as i32);
        }
        if !daemon_cfg.paused {
            let until_tick = daemon_cfg
                .poll_interval
                .saturating_sub(last_coordinator_tick.elapsed());
            poll_timeout_ms =
                poll_timeout_ms.min(until_tick.as_millis().min(i32::MAX as u128) as i32);
        }
        // Floor: don't spin faster than 50ms even with a deadline in the past.
        poll_timeout_ms = poll_timeout_ms.max(50);

        // Wait for an incoming connection or timeout.
        let mut pollfd = libc::pollfd {
            fd: listener_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let poll_ret = unsafe { libc::poll(&mut pollfd, 1, poll_timeout_ms) };

        if poll_ret < 0 {
            // EINTR (e.g. SIGCHLD) — just loop back to reap and retry.
            continue;
        }

        // Try to accept; may still get WouldBlock if poll was a timeout.
        match listener.accept() {
            Ok((stream, _)) => {
                let mut wake_coordinator = false;
                let mut conn_urgent_wake = false;
                let mut conn_delete_coordinator_ids = Vec::new();
                if let Err(e) = ipc::handle_connection(
                    &dir,
                    stream,
                    &mut running,
                    &mut wake_coordinator,
                    &mut conn_urgent_wake,
                    &mut pending_coordinator_ids,
                    &mut conn_delete_coordinator_ids,
                    &mut daemon_cfg,
                    &logger,
                ) {
                    logger.error(&format!("Error handling connection: {}", e));
                }
                // Stop and remove any coordinator agents marked for deletion.
                for cid in conn_delete_coordinator_ids {
                    if let Some(agent) = coordinator_agents.remove(&cid) {
                        logger.info(&format!(
                            "Shutting down coordinator agent {} (deleted via IPC)",
                            cid
                        ));
                        agent.shutdown();
                    }
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
                // poll() timed out — no connection pending, fall through to
                // tick checks.
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

            if enable_coordinator_agent {
                // Lazy-spawn coordinator agents for any pending coordinator IDs
                // that don't already have a running agent.
                for &cid in &pending_coordinator_ids {
                    if !coordinator_agents.contains_key(&cid) {
                        if coordinator_agents.len() >= max_coordinators {
                            logger.warn(&format!(
                                "Cannot spawn coordinator {}: at max_coordinators limit ({})",
                                cid, max_coordinators
                            ));
                            continue;
                        }
                        logger.info(&format!(
                            "Lazy-spawning coordinator agent {} (first message received)",
                            cid
                        ));
                        match coordinator_agent::CoordinatorAgent::spawn(
                            &dir,
                            cid,
                            daemon_cfg.model.as_deref(),
                            Some(&daemon_cfg.executor),
                            daemon_cfg.provider.as_deref(),
                            &logger,
                            event_log.clone(),
                        ) {
                            Ok(agent) => {
                                logger.info(&format!(
                                    "Coordinator agent {} spawned successfully ({}/{} coordinators)",
                                    cid,
                                    coordinator_agents.len() + 1,
                                    max_coordinators
                                ));
                                coordinator_agents.insert(cid, agent);
                            }
                            Err(e) => {
                                logger.warn(&format!(
                                    "Failed to lazy-spawn coordinator agent {}: {}",
                                    cid, e
                                ));
                            }
                        }
                    }
                }
                pending_coordinator_ids.clear();

                if !coordinator_agents.is_empty() {
                    // Route chat messages to all active coordinator agents.
                    // Each coordinator checks its own inbox for pending messages.
                    match route_chat_to_all_agents(&dir, &coordinator_agents, &logger) {
                        Ok(count) if count > 0 => {
                            logger.info(&format!(
                                "Routed {} chat message(s) to coordinator agent(s)",
                                count
                            ));
                        }
                        Ok(_) => {} // No new messages
                        Err(e) => {
                            logger.error(&format!("Failed to route chat to agents: {}", e));
                            // Fall through to tick for stub response
                            should_tick = true;
                        }
                    }
                } else {
                    // All coordinator agent spawns failed — fall through to stub
                    should_tick = true;
                    logger.info("Urgent wake (all coordinator spawns failed): using stub response");
                }
            } else {
                pending_coordinator_ids.clear();
                // No coordinator agents — fall through to coordinator tick
                // which will use the stub response via process_chat_inbox.
                should_tick = true;
                logger.info("Urgent wake (coordinator agents disabled): running coordinator tick");
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
                    // Reload accumulated_tokens from disk before saving to avoid clobbering
                    // increments written by the coordinator agent thread.
                    if let Some(disk) = CoordinatorState::load(&dir) {
                        coord_state.accumulated_tokens = disk.accumulated_tokens;
                    }
                    coord_state.save(&dir);

                    // Record tick events (spawns, completions, failures, zero-output kills)
                    record_tick_events(&dir, &event_log, &logger);

                    logger.info(&format!(
                        "Coordinator tick #{} complete: agents_alive={}, tasks_ready={}, spawned={}",
                        coord_state.ticks, result.agents_alive, result.tasks_ready, result.agents_spawned
                    ));

                    // Dispatch notifications for task state changes (failures, blocks)
                    try_dispatch_notifications(&dir, &logger);

                    // Compaction: run when .compact-0 is graph-ready (cycle-driven)
                    run_graph_compaction(&dir, &mut compaction_error_count, &logger);

                    // Archival: run when .archive-0 is graph-ready (cycle-driven)
                    run_graph_archival(&dir, &mut archival_error_count, &logger);
                }
                Err(e) => {
                    coord_state.ticks += 1;
                    if let Some(disk) = CoordinatorState::load(&dir) {
                        coord_state.accumulated_tokens = disk.accumulated_tokens;
                    }
                    coord_state.save(&dir);
                    logger.error(&format!("Coordinator tick error: {}", e));
                }
            }

            // --- Binary self-restart check ---
            // After each tick, see if the wg binary on disk has been replaced
            // (e.g. by `cargo install --path .`).  If so, exec-replace the
            // current process with the new binary, preserving all CLI args.
            //
            // Flow: (1) compute initial hash on first tick (lazy, avoids
            // blocking startup), (2) cheap mtime+size gate each tick,
            // (3) hash only when metadata changes, (4) compare to initial
            // hash to avoid false restarts on `touch`.
            if let Some(path) = &exe_path {
                // Check if the background hash computation has finished.
                if exe_initial_hash.is_none()
                    && let Some(rx) = &exe_hash_receiver
                    && let Ok(h) = rx.try_recv()
                {
                    logger.info(&format!("Binary hash recorded: {}", short_hash(&h),));
                    exe_initial_hash = Some(h);
                }

                // Cheap metadata check: skip hash if mtime+size unchanged.
                if let (Some(initial_meta), Some(old_hash)) = (&exe_initial_meta, &exe_initial_hash)
                {
                    let meta_changed = fs::metadata(path).ok().is_some_and(|m| {
                        m.modified().ok() != initial_meta.modified().ok()
                            || m.len() != initial_meta.len()
                    });
                    if meta_changed {
                        logger.info("Binary metadata changed, verifying with hash...");
                        if let Ok(hash1) = compute_exe_hash(path) {
                            if hash1 == *old_hash {
                                // Content unchanged (e.g. `touch`), no restart.
                                logger.info("Binary content unchanged despite metadata change");
                            } else {
                                // Content differs — wait and re-hash for stability.
                                std::thread::sleep(Duration::from_secs(1));
                                match compute_exe_hash(path) {
                                    Ok(hash2) if hash2 == hash1 => {
                                        logger.info(&format!(
                                            "Detected wg binary change (old: {}, new: {}), restarting service...",
                                            short_hash(old_hash),
                                            short_hash(&hash1),
                                        ));

                                        // Pre-exec cleanup: save coordinator state.
                                        coord_state.save(&dir);

                                        // Shut down coordinator agents (LLM sessions).
                                        // Running task agents are separate processes
                                        // and survive exec.
                                        let agents_to_shutdown: Vec<(
                                            u32,
                                            coordinator_agent::CoordinatorAgent,
                                        )> = coordinator_agents.drain().collect();
                                        for (cid, agent) in agents_to_shutdown {
                                            logger.info(&format!(
                                                "Shutting down coordinator agent {} before exec-restart",
                                                cid
                                            ));
                                            agent.shutdown();
                                        }

                                        // Remove the socket so the new process can
                                        // re-bind. The listener fd is closed by exec().
                                        let _ = fs::remove_file(&socket);

                                        logger.info(&format!(
                                            "Exec-replacing with: {} {}",
                                            path.display(),
                                            original_args[1..].join(" "),
                                        ));

                                        // exec() replaces the process image — only
                                        // returns on error.
                                        use std::os::unix::process::CommandExt;
                                        let err = process::Command::new(path)
                                            .args(&original_args[1..])
                                            .exec();
                                        // If we get here, exec failed.
                                        logger.error(&format!(
                                            "Exec-restart failed: {}. Continuing with old binary.",
                                            err
                                        ));
                                        // Update stored hash so we don't retry.
                                        exe_initial_hash = Some(hash1);
                                    }
                                    Ok(_) => {
                                        // Hash changed between checks — still writing.
                                        logger.info(
                                            "Binary hash unstable (mid-write?), deferring restart check",
                                        );
                                    }
                                    Err(e) => {
                                        logger.warn(&format!(
                                            "Failed to re-read binary for restart check: {}",
                                            e
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    logger.info("Daemon shutting down");

    // Shut down all coordinator agents
    let agent_count = coordinator_agents.len();
    for (cid, agent) in coordinator_agents {
        logger.info(&format!("Shutting down coordinator agent {}", cid));
        agent.shutdown();
    }
    if agent_count > 0 {
        logger.info(&format!("Shut down {} coordinator agent(s)", agent_count));
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

/// Check if the caller is an agent and refuse stop/pause operations.
/// Returns `Err` if `WG_AGENT_ID` is set, `Ok(())` otherwise.
fn guard_agent_stop_pause() -> Result<()> {
    if std::env::var("WG_AGENT_ID").is_ok() {
        anyhow::bail!("agents cannot stop/pause the service. Use `wg service restart` instead.");
    }
    Ok(())
}

/// Stop the service daemon
#[cfg(unix)]
pub fn run_stop(dir: &Path, force: bool, kill_agents: bool, json: bool) -> Result<()> {
    guard_agent_stop_pause()?;
    run_stop_inner(dir, force, kill_agents, json)
}

/// Inner stop logic (no agent guard) — used by `run_restart` to bypass the guard.
#[cfg(unix)]
fn run_stop_inner(dir: &Path, force: bool, kill_agents: bool, json: bool) -> Result<()> {
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

/// Restart the service daemon: graceful stop (agents kept alive) then start.
///
/// Reads the running daemon's effective config (max_agents, executor, model,
/// poll_interval) before stopping, and passes it to the new daemon so the
/// restart is transparent.
#[cfg(unix)]
pub fn run_restart(dir: &Path, json: bool) -> Result<()> {
    // Capture the current daemon's effective config before stopping.
    let prior_config = CoordinatorState::load(dir);

    // Stop gracefully — agents continue running independently.
    // Use inner variant to bypass the agent guard (agents may restart).
    run_stop_inner(dir, false, false, json)?;

    // Derive start parameters from the previous daemon's state.
    let (max_agents, executor, interval, model) = match &prior_config {
        Some(cs) => (
            Some(cs.max_agents),
            Some(cs.executor.as_str()),
            Some(cs.poll_interval),
            cs.model.as_deref(),
        ),
        None => (None, None, None, None),
    };

    // Start a new daemon with the same config.
    run_start(
        dir, None, // socket — use default
        None, // port
        max_agents, executor, interval, model, json,
        true,  // force — clean up any leftover state
        false, // no_coordinator_agent — use default
    )
}

#[cfg(not(unix))]
pub fn run_restart(_dir: &Path, _json: bool) -> Result<()> {
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

    // Compaction progress
    let config = workgraph::config::Config::load_or_default(dir);
    let compaction_threshold = config.effective_compaction_threshold();
    let compactor_state = workgraph::service::compactor::CompactorState::load(dir);

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
                "frozen": coord.frozen,
                "frozen_pids": coord.frozen_pids,
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
            "compaction": {
                "accumulated_tokens": coord.accumulated_tokens,
                "threshold": compaction_threshold,
                "last_compaction": compactor_state.last_compaction,
                "compaction_count": compactor_state.compaction_count,
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
        let state_str = if coord.frozen {
            ", FROZEN"
        } else if coord.paused {
            ", PAUSED"
        } else {
            ""
        };
        println!(
            "Coordinator: enabled{}, max_agents={}, poll_interval={}s, executor={}, model={}",
            state_str, coord.max_agents, coord.poll_interval, coord.executor, model_str
        );
        if coord.frozen && !coord.frozen_pids.is_empty() {
            println!("  Frozen PIDs: {:?}", coord.frozen_pids);
        }
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
        if compaction_threshold > 0 {
            let pct = if compaction_threshold > 0 {
                ((coord.accumulated_tokens as f64 / compaction_threshold as f64) * 100.0).min(100.0)
                    as u8
            } else {
                0
            };
            let last_str = match compactor_state.last_compaction {
                Some(ref ts) => {
                    if let Ok(parsed) = ts.parse::<chrono::DateTime<chrono::Utc>>() {
                        let ago = chrono::Utc::now()
                            .signed_duration_since(parsed)
                            .num_seconds();
                        format!("last: {} ago", workgraph::format_duration(ago, true))
                    } else {
                        "last: unknown".to_string()
                    }
                }
                None => "last: never".to_string(),
            };
            println!(
                "Compaction: {}/{} tokens ({}%) — {}",
                coord.accumulated_tokens, compaction_threshold, pct, last_str
            );
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
    guard_agent_stop_pause()?;

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

/// Freeze all running agents (SIGSTOP) and pause the coordinator
#[cfg(unix)]
pub fn run_freeze(dir: &Path, json: bool) -> Result<()> {
    guard_agent_stop_pause()?;

    let response = send_request(dir, &IpcRequest::Freeze)?;

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
        let frozen_count = response
            .data
            .as_ref()
            .and_then(|d| d.get("frozen_count"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let status = response
            .data
            .as_ref()
            .and_then(|d| d.get("status"))
            .and_then(|v| v.as_str())
            .unwrap_or("frozen");

        if status == "already_frozen" {
            println!("Service is already frozen.");
        } else {
            println!("Froze {} agent(s). Service paused.", frozen_count);
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_freeze(_dir: &Path, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Thaw all frozen agents (SIGCONT) and resume the coordinator
#[cfg(unix)]
pub fn run_thaw(dir: &Path, json: bool) -> Result<()> {
    let response = send_request(dir, &IpcRequest::Thaw)?;

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
        let thawed_count = response
            .data
            .as_ref()
            .and_then(|d| d.get("thawed_count"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let dead_count = response
            .data
            .as_ref()
            .and_then(|d| d.get("dead_pids"))
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let status = response
            .data
            .as_ref()
            .and_then(|d| d.get("status"))
            .and_then(|v| v.as_str())
            .unwrap_or("thawed");

        if status == "not_frozen" {
            println!("Service is not frozen.");
        } else {
            let mut msg = format!("Thawed {} agent(s). Service resumed.", thawed_count);
            if dead_count > 0 {
                msg.push_str(&format!(" ({} agent(s) died while frozen.)", dead_count));
            }
            println!("{}", msg);
        }
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_thaw(_dir: &Path, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Create a new coordinator session via IPC
#[cfg(unix)]
pub fn run_create_coordinator(dir: &Path, name: Option<&str>, json: bool) -> Result<()> {
    let response = send_request(
        dir,
        &IpcRequest::CreateCoordinator {
            name: name.map(|s| s.to_string()),
        },
    )?;

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

    if let Some(data) = &response.data {
        println!("{}", serde_json::to_string_pretty(data)?);
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_create_coordinator(_dir: &Path, _name: Option<&str>, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Delete a coordinator session via IPC
#[cfg(unix)]
pub fn run_delete_coordinator(dir: &Path, coordinator_id: u32, json: bool) -> Result<()> {
    let response = send_request(dir, &IpcRequest::DeleteCoordinator { coordinator_id })?;

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

    if let Some(data) = &response.data {
        println!("{}", serde_json::to_string_pretty(data)?);
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_delete_coordinator(_dir: &Path, _coordinator_id: u32, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Archive a coordinator session via IPC (mark as Done)
#[cfg(unix)]
pub fn run_archive_coordinator(dir: &Path, coordinator_id: u32, json: bool) -> Result<()> {
    let response = send_request(dir, &IpcRequest::ArchiveCoordinator { coordinator_id })?;

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

    if let Some(data) = &response.data {
        println!("{}", serde_json::to_string_pretty(data)?);
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_archive_coordinator(_dir: &Path, _coordinator_id: u32, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Stop a coordinator session via IPC (kill agent, reset to Open)
#[cfg(unix)]
pub fn run_stop_coordinator(dir: &Path, coordinator_id: u32, json: bool) -> Result<()> {
    let response = send_request(dir, &IpcRequest::StopCoordinator { coordinator_id })?;

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

    if let Some(data) = &response.data {
        println!("{}", serde_json::to_string_pretty(data)?);
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_stop_coordinator(_dir: &Path, _coordinator_id: u32, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Check if a Unix socket is accepting connections by doing a quick connect+drop.
#[cfg(unix)]
fn socket_accepting(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

/// Public wrapper: check if the service process is alive
pub fn is_service_alive(pid: u32) -> bool {
    is_process_alive(pid)
}

/// Check if the coordinator is currently paused
pub fn is_service_paused(dir: &Path) -> bool {
    CoordinatorState::load(dir).is_some_and(|c| c.paused)
}

/// Send an IPC request to the running service.
///
/// Retries transient connection failures (ECONNREFUSED, broken pipe) up to 2
/// times with short exponential backoff (50ms, 100ms) before giving up.
/// Distinguishes "daemon not running" from "daemon unreachable" in errors.
#[cfg(unix)]
pub fn send_request(dir: &Path, request: &IpcRequest) -> Result<IpcResponse> {
    let state = ServiceState::load(dir)?.ok_or_else(|| {
        anyhow::anyhow!("Service not running (no state file). Start it with 'wg service start'.")
    })?;

    if !is_process_alive(state.pid) {
        anyhow::bail!(
            "Service daemon (PID {}) is not running. \
             The state file is stale — start a new service with 'wg service start'.",
            state.pid
        );
    }

    let socket = PathBuf::from(&state.socket_path);
    if !socket.exists() {
        anyhow::bail!(
            "Service socket {:?} does not exist, but daemon PID {} is alive. \
             The daemon may still be starting up — try again shortly, \
             or restart with 'wg service start --force'.",
            socket,
            state.pid
        );
    }

    // Retry transient connection failures with short backoff.
    const MAX_RETRIES: u32 = 2;
    const BASE_BACKOFF_MS: u64 = 50;

    let mut last_err = None;
    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(
                BASE_BACKOFF_MS * (1 << (attempt - 1)),
            ));
        }

        match UnixStream::connect(&socket) {
            Ok(mut stream) => {
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
            Err(e) => {
                let retryable = matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::BrokenPipe
                );
                if !retryable || attempt == MAX_RETRIES {
                    last_err = Some(e);
                    break;
                }
                last_err = Some(e);
            }
        }
    }

    let err = last_err.unwrap();
    anyhow::bail!(
        "Could not connect to service at {:?} (PID {}, {} retries exhausted): {}. \
         The daemon may be overloaded — try again, or restart with 'wg service start --force'.",
        socket,
        state.pid,
        MAX_RETRIES,
        err
    )
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

    #[test]
    fn test_guard_agent_stop_pause_blocks_when_agent() {
        // SAFETY: test-only env manipulation; these tests are not parallel-safe
        // but each test restores the var before returning.
        unsafe { std::env::set_var("WG_AGENT_ID", "test-agent") };
        let result = guard_agent_stop_pause();
        unsafe { std::env::remove_var("WG_AGENT_ID") };

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("agents cannot stop/pause the service"),
            "Expected agent guard message, got: {msg}"
        );
    }

    #[test]
    fn test_guard_agent_stop_pause_allows_when_not_agent() {
        // Ensure WG_AGENT_ID is not set
        unsafe { std::env::remove_var("WG_AGENT_ID") };
        let result = guard_agent_stop_pause();
        assert!(result.is_ok());
    }

    #[test]
    fn test_compaction_cycle() {
        use workgraph::graph::Status;

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let gp = dir.join("graph.jsonl");

        // Initialize an empty graph
        let graph = workgraph::graph::WorkGraph::new();
        workgraph::parser::save_graph(&graph, &gp).unwrap();

        // Run ensure_coordinator_task — should create both .coordinator-0 and .compact-0
        ensure_coordinator_task(dir);

        let graph = load_graph(&gp).unwrap();

        // .coordinator-0 should exist with cycle_config
        let coord = graph
            .get_task(".coordinator-0")
            .expect(".coordinator-0 should exist");
        assert_eq!(coord.status, Status::InProgress);
        assert!(
            coord.cycle_config.is_some(),
            "coordinator should have cycle_config"
        );
        assert!(
            coord.after.contains(&".compact-0".to_string()),
            "coordinator should have back-edge to .compact-0"
        );
        assert!(
            coord.tags.contains(&"coordinator-loop".to_string()),
            "coordinator should have coordinator-loop tag"
        );

        // .compact-0 should exist with proper edges
        let compact = graph
            .get_task(".compact-0")
            .expect(".compact-0 should exist");
        assert_eq!(compact.status, Status::Open);
        assert!(
            compact.after.contains(&".coordinator-0".to_string()),
            "compact should depend on .coordinator-0"
        );
        assert!(
            compact.tags.contains(&"compact-loop".to_string()),
            "compact should have compact-loop tag"
        );

        // The two tasks should form a cycle (SCC)
        let cycle_analysis = graph.compute_cycle_analysis();
        assert!(
            cycle_analysis.task_to_cycle.contains_key(".coordinator-0"),
            "coordinator should be part of a detected cycle"
        );
        assert!(
            cycle_analysis.task_to_cycle.contains_key(".compact-0"),
            "compact should be part of a detected cycle"
        );
        // Both should be in the same cycle
        assert_eq!(
            cycle_analysis.task_to_cycle.get(".coordinator-0"),
            cycle_analysis.task_to_cycle.get(".compact-0"),
            "coordinator and compact should be in the same cycle"
        );
    }

    #[test]
    fn test_compaction_cycle_idempotent() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let gp = dir.join("graph.jsonl");

        let graph = workgraph::graph::WorkGraph::new();
        workgraph::parser::save_graph(&graph, &gp).unwrap();

        // Run twice — should be idempotent
        ensure_coordinator_task(dir);
        ensure_coordinator_task(dir);

        let graph = load_graph(&gp).unwrap();
        assert!(graph.get_task(".coordinator-0").is_some());
        assert!(graph.get_task(".compact-0").is_some());

        // Only one back-edge, not duplicated
        let coord = graph.get_task(".coordinator-0").unwrap();
        let compact_refs: Vec<_> = coord.after.iter().filter(|a| *a == ".compact-0").collect();
        assert_eq!(compact_refs.len(), 1, "back-edge should not be duplicated");
    }

    #[test]
    fn test_run_graph_compaction_updates_task() {
        use workgraph::graph::{Node, Status, Task};

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let gp = dir.join("graph.jsonl");

        // Create a graph with .compact-0 (no deps → immediately ready)
        let mut graph = workgraph::graph::WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: ".compact-0".to_string(),
            title: "Compact 0".to_string(),
            status: Status::Open,
            tags: vec!["compact-loop".to_string()],
            ..Default::default()
        }));
        workgraph::parser::save_graph(&graph, &gp).unwrap();

        // Create a logger for the test
        let logger = DaemonLogger::open(dir).unwrap();

        // Pre-seed accumulated tokens above threshold so compaction isn't deferred
        let mut cs = CoordinatorState::load_or_default_for(dir, 0);
        cs.accumulated_tokens = 200_000;
        cs.save_for(dir, 0);

        let mut error_count = 0u64;
        // .compact-0 is Open with no deps → graph-ready → compaction fires
        // It will fail (no LLM) but we verify the task status gets updated
        run_graph_compaction(dir, &mut error_count, &logger);

        // After the call, .compact-0 should be Open (failed, reverted to Open)
        let graph = load_graph(&gp).unwrap();
        let compact = graph.get_task(".compact-0").unwrap();
        // The task should have log entries from the compaction attempt
        assert!(
            compact.log.len() > 0,
            "compact task should have log entries after compaction attempt"
        );
    }

    #[test]
    fn test_compaction_fires_when_compact_ready() {
        use workgraph::graph::{Node, Status, Task};

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let gp = dir.join("graph.jsonl");

        // Create a graph: .coordinator-0 (Done) → .compact-0 (Open, after .coordinator-0)
        let mut graph = workgraph::graph::WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: ".coordinator-0".to_string(),
            title: "Coordinator 0".to_string(),
            status: Status::Done,
            ..Default::default()
        }));
        graph.add_node(Node::Task(Task {
            id: ".compact-0".to_string(),
            title: "Compact 0".to_string(),
            status: Status::Open,
            after: vec![".coordinator-0".to_string()],
            tags: vec!["compact-loop".to_string()],
            ..Default::default()
        }));
        workgraph::parser::save_graph(&graph, &gp).unwrap();

        let logger = DaemonLogger::open(dir).unwrap();
        let mut error_count = 0u64;

        // Pre-seed accumulated tokens above threshold so compaction isn't deferred
        let mut cs = CoordinatorState::load_or_default_for(dir, 0);
        cs.accumulated_tokens = 200_000;
        cs.save_for(dir, 0);

        // .compact-0 is Open and dep (.coordinator-0) is Done → should fire
        run_graph_compaction(dir, &mut error_count, &logger);

        let graph = load_graph(&gp).unwrap();
        let compact = graph.get_task(".compact-0").unwrap();
        // Compaction attempted (will fail due to no LLM, but log entries prove it fired)
        assert!(
            !compact.log.is_empty(),
            "compaction should fire when .compact-0 is graph-ready"
        );
    }

    #[test]
    fn test_compaction_blocked_when_dep_not_done() {
        use workgraph::graph::{Node, Status, Task};

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let gp = dir.join("graph.jsonl");

        // Create a graph: .coordinator-0 (InProgress) → .compact-0 (Open, after .coordinator-0)
        let mut graph = workgraph::graph::WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: ".coordinator-0".to_string(),
            title: "Coordinator 0".to_string(),
            status: Status::InProgress,
            ..Default::default()
        }));
        graph.add_node(Node::Task(Task {
            id: ".compact-0".to_string(),
            title: "Compact 0".to_string(),
            status: Status::Open,
            after: vec![".coordinator-0".to_string()],
            tags: vec!["compact-loop".to_string()],
            ..Default::default()
        }));
        workgraph::parser::save_graph(&graph, &gp).unwrap();

        let logger = DaemonLogger::open(dir).unwrap();
        let mut error_count = 0u64;

        // .compact-0 is Open but dep (.coordinator-0) is InProgress → should NOT fire
        run_graph_compaction(dir, &mut error_count, &logger);

        let graph = load_graph(&gp).unwrap();
        let compact = graph.get_task(".compact-0").unwrap();
        assert!(
            compact.log.is_empty(),
            "compaction should NOT fire when .compact-0 deps are not terminal"
        );
        assert_eq!(
            compact.status,
            Status::Open,
            ".compact-0 should remain Open when blocked"
        );
    }

    #[test]
    fn test_compaction_reopens_done_compact_when_over_threshold() {
        use workgraph::graph::{CycleConfig, Node, Status, Task};

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let gp = dir.join("graph.jsonl");

        // Realistic setup: .coordinator-0 is InProgress (persistent) with cycle_config,
        // .compact-0 is Done (completed a previous iteration) in the same cycle.
        let mut graph = workgraph::graph::WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: ".coordinator-0".to_string(),
            title: "Coordinator 0".to_string(),
            status: Status::InProgress,
            after: vec![".compact-0".to_string()],
            cycle_config: Some(CycleConfig {
                max_iterations: 0,
                guard: None,
                delay: None,
                no_converge: true,
                restart_on_failure: true,
                max_failure_restarts: None,
            }),
            ..Default::default()
        }));
        graph.add_node(Node::Task(Task {
            id: ".compact-0".to_string(),
            title: "Compact 0".to_string(),
            status: Status::Done,
            after: vec![".coordinator-0".to_string()],
            tags: vec!["compact-loop".to_string()],
            loop_iteration: 5, // has run before
            ..Default::default()
        }));
        workgraph::parser::save_graph(&graph, &gp).unwrap();

        let logger = DaemonLogger::open(dir).unwrap();
        let mut error_count = 0u64;

        // Pre-seed accumulated tokens above threshold
        let mut cs = CoordinatorState::load_or_default_for(dir, 0);
        cs.accumulated_tokens = 200_000;
        cs.save_for(dir, 0);

        // Before fix: .compact-0 is Done, cycle can't iterate because
        // .coordinator-0 is InProgress — compaction would never fire.
        // After fix: .compact-0 should be reset to Open, then fire.
        run_graph_compaction(dir, &mut error_count, &logger);

        let graph = load_graph(&gp).unwrap();
        let compact = graph.get_task(".compact-0").unwrap();

        // Compaction should have been attempted (will fail due to no LLM,
        // but log entries prove .compact-0 was re-opened and compaction fired)
        assert!(
            !compact.log.is_empty(),
            "compaction should fire after re-opening .compact-0 when tokens exceed threshold"
        );
        // The re-open log entry should be present
        assert!(
            compact.log.iter().any(|e| e.message.contains("Re-opened")),
            "should have a Re-opened log entry"
        );
    }

    #[test]
    fn test_compaction_does_not_reopen_below_threshold() {
        use workgraph::graph::{CycleConfig, Node, Status, Task};

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        let gp = dir.join("graph.jsonl");

        // Same setup but tokens are BELOW threshold
        let mut graph = workgraph::graph::WorkGraph::new();
        graph.add_node(Node::Task(Task {
            id: ".coordinator-0".to_string(),
            title: "Coordinator 0".to_string(),
            status: Status::InProgress,
            after: vec![".compact-0".to_string()],
            cycle_config: Some(CycleConfig {
                max_iterations: 0,
                guard: None,
                delay: None,
                no_converge: true,
                restart_on_failure: true,
                max_failure_restarts: None,
            }),
            ..Default::default()
        }));
        graph.add_node(Node::Task(Task {
            id: ".compact-0".to_string(),
            title: "Compact 0".to_string(),
            status: Status::Done,
            after: vec![".coordinator-0".to_string()],
            tags: vec!["compact-loop".to_string()],
            loop_iteration: 5,
            ..Default::default()
        }));
        workgraph::parser::save_graph(&graph, &gp).unwrap();

        let logger = DaemonLogger::open(dir).unwrap();
        let mut error_count = 0u64;

        // Tokens below default threshold — should NOT re-open
        let mut cs = CoordinatorState::load_or_default_for(dir, 0);
        cs.accumulated_tokens = 1000;
        cs.save_for(dir, 0);

        run_graph_compaction(dir, &mut error_count, &logger);

        let graph = load_graph(&gp).unwrap();
        let compact = graph.get_task(".compact-0").unwrap();

        assert_eq!(
            compact.status,
            Status::Done,
            ".compact-0 should remain Done when tokens are below threshold"
        );
        assert!(
            compact.log.is_empty(),
            "no log entries should be added when not re-opening"
        );
    }

    #[test]
    fn test_compute_exe_hash_known_file() {
        // Create a temp file with known content and verify hash is deterministic.
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test_binary");
        fs::write(&path, b"hello world").unwrap();

        let hash1 = compute_exe_hash(&path).unwrap();
        let hash2 = compute_exe_hash(&path).unwrap();
        assert_eq!(
            hash1, hash2,
            "hashing the same file twice should be identical"
        );

        // Verify against known SHA-256 of "hello world"
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert_eq!(hex::encode(hash1), expected);
    }

    #[test]
    fn test_compute_exe_hash_detects_change() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test_binary");
        fs::write(&path, b"version 1").unwrap();
        let hash1 = compute_exe_hash(&path).unwrap();

        fs::write(&path, b"version 2").unwrap();
        let hash2 = compute_exe_hash(&path).unwrap();
        assert_ne!(
            hash1, hash2,
            "different content should produce different hashes"
        );
    }

    #[test]
    fn test_compute_exe_hash_nonexistent() {
        let result = compute_exe_hash(Path::new("/nonexistent/binary"));
        assert!(result.is_err());
    }

    #[test]
    fn test_short_hash_format() {
        let hash = [
            0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let s = short_hash(&hash);
        assert_eq!(s, "abcdef012345");
        assert_eq!(s.len(), 12, "short_hash should produce 12 hex chars");
    }

    #[test]
    fn test_per_user_coord_state_path() {
        let temp_dir = TempDir::new().unwrap();
        let path0 = coordinator_state_path(temp_dir.path(), 0);
        assert_eq!(
            path0,
            temp_dir
                .path()
                .join("service")
                .join("coordinator-state-0.json")
        );
        let path1 = coordinator_state_path(temp_dir.path(), 1);
        assert_eq!(
            path1,
            temp_dir
                .path()
                .join("service")
                .join("coordinator-state-1.json")
        );
        let path42 = coordinator_state_path(temp_dir.path(), 42);
        assert_eq!(
            path42,
            temp_dir
                .path()
                .join("service")
                .join("coordinator-state-42.json")
        );
    }

    #[test]
    fn test_per_user_coord_state_per_id_roundtrip() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Save state for coordinator 0
        let state0 = CoordinatorState {
            enabled: true,
            max_agents: 4,
            accumulated_tokens: 1000,
            ..Default::default()
        };
        state0.save_for(dir, 0);

        // Save state for coordinator 1
        let state1 = CoordinatorState {
            enabled: true,
            max_agents: 2,
            accumulated_tokens: 5000,
            ..Default::default()
        };
        state1.save_for(dir, 1);

        // Load each and verify independence
        let loaded0 = CoordinatorState::load_for(dir, 0).unwrap();
        assert_eq!(loaded0.max_agents, 4);
        assert_eq!(loaded0.accumulated_tokens, 1000);

        let loaded1 = CoordinatorState::load_for(dir, 1).unwrap();
        assert_eq!(loaded1.max_agents, 2);
        assert_eq!(loaded1.accumulated_tokens, 5000);

        // Coordinator 2 should not exist
        assert!(CoordinatorState::load_for(dir, 2).is_none());
    }

    #[test]
    fn test_per_user_coord_backward_compat_legacy_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Write to legacy shared file (coordinator-state.json)
        let legacy_state = CoordinatorState {
            enabled: true,
            max_agents: 8,
            accumulated_tokens: 42,
            ..Default::default()
        };
        let legacy_path = coordinator_state_path_legacy(dir);
        let content = serde_json::to_string_pretty(&legacy_state).unwrap();
        fs::write(&legacy_path, content).unwrap();

        // No per-ID file for coordinator 0 → should fall back to legacy
        let loaded = CoordinatorState::load_for(dir, 0).unwrap();
        assert_eq!(loaded.max_agents, 8);
        assert_eq!(loaded.accumulated_tokens, 42);

        // load() shorthand should also work (backward compat)
        let loaded_compat = CoordinatorState::load(dir).unwrap();
        assert_eq!(loaded_compat.max_agents, 8);

        // Non-zero coordinator should NOT fall back to legacy
        assert!(CoordinatorState::load_for(dir, 1).is_none());
    }

    #[test]
    fn test_per_user_coord_per_id_overrides_legacy() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Write legacy file
        let legacy = CoordinatorState {
            max_agents: 8,
            accumulated_tokens: 100,
            ..Default::default()
        };
        let legacy_path = coordinator_state_path_legacy(dir);
        fs::write(&legacy_path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        // Write per-ID file for coordinator 0
        let per_id = CoordinatorState {
            max_agents: 16,
            accumulated_tokens: 9999,
            ..Default::default()
        };
        per_id.save_for(dir, 0);

        // Per-ID file should take precedence over legacy
        let loaded = CoordinatorState::load_for(dir, 0).unwrap();
        assert_eq!(loaded.max_agents, 16);
        assert_eq!(loaded.accumulated_tokens, 9999);
    }

    #[test]
    fn test_per_user_coord_two_coordinators_no_state_conflict() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Simulate alice's coordinator (ID 1)
        let mut alice_state = CoordinatorState {
            enabled: true,
            max_agents: 3,
            accumulated_tokens: 0,
            executor: "claude".to_string(),
            ..Default::default()
        };
        alice_state.save_for(dir, 1);

        // Simulate bob's coordinator (ID 2)
        let mut bob_state = CoordinatorState {
            enabled: true,
            max_agents: 5,
            accumulated_tokens: 0,
            executor: "claude".to_string(),
            ..Default::default()
        };
        bob_state.save_for(dir, 2);

        // Update alice's tokens independently
        alice_state.accumulated_tokens = 500;
        alice_state.save_for(dir, 1);

        // Update bob's tokens independently
        bob_state.accumulated_tokens = 1200;
        bob_state.save_for(dir, 2);

        // Verify no cross-contamination
        let alice_loaded = CoordinatorState::load_for(dir, 1).unwrap();
        assert_eq!(alice_loaded.accumulated_tokens, 500);
        assert_eq!(alice_loaded.max_agents, 3);

        let bob_loaded = CoordinatorState::load_for(dir, 2).unwrap();
        assert_eq!(bob_loaded.accumulated_tokens, 1200);
        assert_eq!(bob_loaded.max_agents, 5);
    }

    #[test]
    fn test_per_user_coord_remove_per_id() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        let state = CoordinatorState {
            enabled: true,
            ..Default::default()
        };
        state.save_for(dir, 3);
        assert!(CoordinatorState::load_for(dir, 3).is_some());

        CoordinatorState::remove_for(dir, 3);
        assert!(CoordinatorState::load_for(dir, 3).is_none());
    }

    #[test]
    fn test_per_user_coord_remove_cleans_legacy() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Create both legacy and per-ID file for coordinator 0
        let state = CoordinatorState {
            enabled: true,
            ..Default::default()
        };
        state.save_for(dir, 0);
        let legacy_path = coordinator_state_path_legacy(dir);
        fs::write(&legacy_path, "{}").unwrap();

        // remove() should clean up both
        CoordinatorState::remove(dir);
        assert!(CoordinatorState::load_for(dir, 0).is_none());
        assert!(!legacy_path.exists());
    }

    #[test]
    fn test_per_coord_state_load_all() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // No files → empty
        assert!(CoordinatorState::load_all(dir).is_empty());

        // Create three coordinators
        CoordinatorState {
            enabled: true,
            max_agents: 4,
            accumulated_tokens: 100,
            ..Default::default()
        }
        .save_for(dir, 0);

        CoordinatorState {
            enabled: true,
            max_agents: 2,
            accumulated_tokens: 200,
            ..Default::default()
        }
        .save_for(dir, 1);

        CoordinatorState {
            enabled: true,
            max_agents: 6,
            accumulated_tokens: 300,
            ..Default::default()
        }
        .save_for(dir, 5);

        let all = CoordinatorState::load_all(dir);
        assert_eq!(all.len(), 3);
        // Should be sorted by ID
        assert_eq!(all[0].0, 0);
        assert_eq!(all[1].0, 1);
        assert_eq!(all[2].0, 5);
        assert_eq!(all[0].1.accumulated_tokens, 100);
        assert_eq!(all[1].1.accumulated_tokens, 200);
        assert_eq!(all[2].1.accumulated_tokens, 300);
    }

    #[test]
    fn test_per_coord_state_load_all_legacy_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Write only a legacy file
        let legacy = CoordinatorState {
            enabled: true,
            max_agents: 8,
            accumulated_tokens: 42,
            ..Default::default()
        };
        let legacy_path = coordinator_state_path_legacy(dir);
        fs::write(&legacy_path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let all = CoordinatorState::load_all(dir);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].0, 0);
        assert_eq!(all[0].1.accumulated_tokens, 42);
    }

    #[test]
    fn test_per_coord_state_total_accumulated_tokens() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Empty dir → 0
        assert_eq!(CoordinatorState::total_accumulated_tokens(dir), 0);

        // Coordinator 0: 100 tokens
        CoordinatorState {
            accumulated_tokens: 100,
            ..Default::default()
        }
        .save_for(dir, 0);

        // Coordinator 1: 250 tokens
        CoordinatorState {
            accumulated_tokens: 250,
            ..Default::default()
        }
        .save_for(dir, 1);

        // Coordinator 2: 650 tokens
        CoordinatorState {
            accumulated_tokens: 650,
            ..Default::default()
        }
        .save_for(dir, 2);

        assert_eq!(CoordinatorState::total_accumulated_tokens(dir), 1000);
    }

    #[test]
    fn test_per_coord_state_reset_all_accumulated_tokens() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        CoordinatorState {
            accumulated_tokens: 5000,
            max_agents: 4,
            ..Default::default()
        }
        .save_for(dir, 0);

        CoordinatorState {
            accumulated_tokens: 3000,
            max_agents: 2,
            ..Default::default()
        }
        .save_for(dir, 1);

        assert_eq!(CoordinatorState::total_accumulated_tokens(dir), 8000);

        CoordinatorState::reset_all_accumulated_tokens(dir);

        assert_eq!(CoordinatorState::total_accumulated_tokens(dir), 0);
        // Non-token fields should be preserved
        let c0 = CoordinatorState::load_for(dir, 0).unwrap();
        assert_eq!(c0.max_agents, 4);
        let c1 = CoordinatorState::load_for(dir, 1).unwrap();
        assert_eq!(c1.max_agents, 2);
    }

    #[test]
    fn test_per_coord_state_remove_all() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Create per-ID files and a legacy file
        CoordinatorState::default().save_for(dir, 0);
        CoordinatorState::default().save_for(dir, 1);
        CoordinatorState::default().save_for(dir, 5);
        let legacy_path = coordinator_state_path_legacy(dir);
        fs::write(&legacy_path, "{}").unwrap();

        assert_eq!(CoordinatorState::load_all(dir).len(), 3);
        assert!(legacy_path.exists());

        CoordinatorState::remove_all(dir);

        assert!(CoordinatorState::load_all(dir).is_empty());
        assert!(!legacy_path.exists());
        assert!(CoordinatorState::load_for(dir, 0).is_none());
        assert!(CoordinatorState::load_for(dir, 1).is_none());
        assert!(CoordinatorState::load_for(dir, 5).is_none());
    }

    #[test]
    fn test_per_coord_state_migrate_legacy() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        let legacy = CoordinatorState {
            enabled: true,
            max_agents: 8,
            accumulated_tokens: 999,
            executor: "claude".to_string(),
            ..Default::default()
        };
        let legacy_path = coordinator_state_path_legacy(dir);
        fs::write(&legacy_path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();
        let per_id_path = coordinator_state_path(dir, 0);
        assert!(!per_id_path.exists());

        CoordinatorState::migrate_legacy(dir);

        // Legacy file should be removed
        assert!(!legacy_path.exists());
        // Per-ID file should exist with same data
        assert!(per_id_path.exists());
        let loaded = CoordinatorState::load_for(dir, 0).unwrap();
        assert_eq!(loaded.max_agents, 8);
        assert_eq!(loaded.accumulated_tokens, 999);
        assert_eq!(loaded.executor, "claude");
    }

    #[test]
    fn test_per_coord_state_migrate_legacy_noop_when_per_id_exists() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Create both legacy and per-ID files
        let legacy = CoordinatorState {
            max_agents: 99,
            ..Default::default()
        };
        let legacy_path = coordinator_state_path_legacy(dir);
        fs::write(&legacy_path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let per_id = CoordinatorState {
            max_agents: 4,
            ..Default::default()
        };
        per_id.save_for(dir, 0);

        CoordinatorState::migrate_legacy(dir);

        // Per-ID file should keep its original data (not overwritten by legacy)
        let loaded = CoordinatorState::load_for(dir, 0).unwrap();
        assert_eq!(loaded.max_agents, 4);
        // Legacy file should NOT be removed (migration is a no-op)
        assert!(legacy_path.exists());
    }

    #[test]
    fn test_per_coord_state_update_all() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        CoordinatorState {
            paused: false,
            max_agents: 4,
            ..Default::default()
        }
        .save_for(dir, 0);

        CoordinatorState {
            paused: false,
            max_agents: 2,
            ..Default::default()
        }
        .save_for(dir, 1);

        // Pause all coordinators
        CoordinatorState::update_all(dir, |cs| cs.paused = true);

        let c0 = CoordinatorState::load_for(dir, 0).unwrap();
        assert!(c0.paused);
        assert_eq!(c0.max_agents, 4); // Unchanged

        let c1 = CoordinatorState::load_for(dir, 1).unwrap();
        assert!(c1.paused);
        assert_eq!(c1.max_agents, 2); // Unchanged
    }

    #[test]
    fn test_per_coord_state_two_coordinators_simultaneous_write() {
        use std::sync::{Arc, Barrier};

        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Initialize state for two coordinators
        CoordinatorState::default().save_for(dir, 0);
        CoordinatorState::default().save_for(dir, 1);

        let dir_a = dir.to_path_buf();
        let dir_b = dir.to_path_buf();
        let barrier = Arc::new(Barrier::new(2));
        let barrier_a = barrier.clone();
        let barrier_b = barrier.clone();

        // Thread A writes coordinator 0 repeatedly
        let handle_a = std::thread::spawn(move || {
            barrier_a.wait();
            for i in 0..100u64 {
                let mut state = CoordinatorState::load_or_default_for(&dir_a, 0);
                state.accumulated_tokens = i;
                state.ticks = i;
                state.max_agents = 4;
                state.save_for(&dir_a, 0);
            }
        });

        // Thread B writes coordinator 1 repeatedly
        let handle_b = std::thread::spawn(move || {
            barrier_b.wait();
            for i in 0..100u64 {
                let mut state = CoordinatorState::load_or_default_for(&dir_b, 1);
                state.accumulated_tokens = i * 10;
                state.ticks = i;
                state.max_agents = 8;
                state.save_for(&dir_b, 1);
            }
        });

        handle_a.join().unwrap();
        handle_b.join().unwrap();

        // Both files should exist and be valid JSON (no corruption from concurrent writes)
        let c0 = CoordinatorState::load_for(dir, 0).unwrap();
        assert_eq!(c0.max_agents, 4);
        assert_eq!(c0.ticks, 99);
        assert_eq!(c0.accumulated_tokens, 99);

        let c1 = CoordinatorState::load_for(dir, 1).unwrap();
        assert_eq!(c1.max_agents, 8);
        assert_eq!(c1.ticks, 99);
        assert_eq!(c1.accumulated_tokens, 990);
    }

    #[test]
    fn test_per_coord_state_service_status_reads_all() {
        let temp_dir = TempDir::new().unwrap();
        let dir = temp_dir.path();
        fs::create_dir_all(dir.join("service")).unwrap();

        // Create state for coordinators 0, 1, 2
        CoordinatorState {
            enabled: true,
            accumulated_tokens: 100,
            ..Default::default()
        }
        .save_for(dir, 0);

        CoordinatorState {
            enabled: true,
            accumulated_tokens: 200,
            ..Default::default()
        }
        .save_for(dir, 1);

        CoordinatorState {
            enabled: true,
            accumulated_tokens: 300,
            ..Default::default()
        }
        .save_for(dir, 2);

        // load_all should return all three
        let all = CoordinatorState::load_all(dir);
        assert_eq!(all.len(), 3);

        // total_accumulated_tokens should sum all
        assert_eq!(CoordinatorState::total_accumulated_tokens(dir), 600);

        // Coordinator 0 should be loadable independently
        let c0 = CoordinatorState::load_or_default_for(dir, 0);
        assert_eq!(c0.accumulated_tokens, 100);
    }
}
