//! Agent Service Daemon
//!
//! Manages the wg service daemon that coordinates agent spawning and monitoring.
//!
//! Usage:
//!   wg service start [--port N] [--socket PATH]  # Start the service daemon
//!   wg service stop [--force]                    # Stop the service daemon
//!   wg service status                            # Show service status

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

use workgraph::service::AgentRegistry;

/// Default socket path (project-specific)
pub fn default_socket_path(dir: &Path) -> PathBuf {
    // Use project directory name for unique socket
    let project_name = dir
        .canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "wg".to_string());
    PathBuf::from(format!("/tmp/wg-{}.sock", project_name))
}

/// Path to the PID file for the service
pub fn pid_file_path(dir: &Path) -> PathBuf {
    dir.join("service").join("daemon.pid")
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
            fs::create_dir_all(&service_dir)
                .with_context(|| format!("Failed to create service directory at {:?}", service_dir))?;
        }
        let path = state_file_path(dir);
        let content = serde_json::to_string_pretty(self)
            .context("Failed to serialize service state")?;
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

/// IPC Request types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Spawn a new agent for a task
    Spawn {
        task_id: String,
        executor: String,
        #[serde(default)]
        timeout: Option<String>,
    },
    /// List all agents
    Agents,
    /// Kill an agent
    Kill {
        agent_id: String,
        #[serde(default)]
        force: bool,
    },
    /// Record heartbeat for an agent
    Heartbeat { agent_id: String },
    /// Get service status
    Status,
    /// Shutdown the service
    Shutdown {
        #[serde(default)]
        force: bool,
    },
}

/// IPC Response types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(flatten)]
    pub data: Option<serde_json::Value>,
}

impl IpcResponse {
    pub fn success(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            error: None,
            data: Some(data),
        }
    }

    pub fn error(msg: &str) -> Self {
        Self {
            ok: false,
            error: Some(msg.to_string()),
            data: None,
        }
    }
}

/// Start the service daemon
#[cfg(unix)]
pub fn run_start(dir: &Path, socket_path: Option<&str>, _port: Option<u16>, json: bool) -> Result<()> {
    // Check if service is already running
    if let Some(state) = ServiceState::load(dir)? {
        if is_process_running(state.pid) {
            if json {
                let output = serde_json::json!({
                    "error": "Service already running",
                    "pid": state.pid,
                    "socket": state.socket_path,
                });
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                println!("Service already running (PID {})", state.pid);
                println!("Socket: {}", state.socket_path);
            }
            return Ok(());
        }
        // Stale state, clean up
        ServiceState::remove(dir)?;
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
    let current_exe = std::env::current_exe()
        .context("Failed to get current executable path")?;

    let dir_str = dir.to_string_lossy().to_string();
    let socket_str = socket.to_string_lossy().to_string();

    // Start daemon in background
    let child = process::Command::new(&current_exe)
        .args(["--dir", &dir_str, "service", "daemon", "--socket", &socket_str])
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
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
    if !is_process_running(pid) {
        ServiceState::remove(dir)?;
        anyhow::bail!("Daemon process exited immediately. Check logs.");
    }

    if json {
        let output = serde_json::json!({
            "status": "started",
            "pid": pid,
            "socket": socket_str,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Service started (PID {})", pid);
        println!("Socket: {}", socket_str);
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_start(_dir: &Path, _socket_path: Option<&str>, _port: Option<u16>, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Run the actual daemon loop (called by forked process)
#[cfg(unix)]
pub fn run_daemon(dir: &Path, socket_path: &str) -> Result<()> {
    let socket = PathBuf::from(socket_path);

    // Ensure socket directory exists
    if let Some(parent) = socket.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)?;
        }
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

    while running {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = handle_connection(&dir, stream, &mut running) {
                    eprintln!("Error handling connection: {}", e);
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No connection, sleep briefly
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!("Accept error: {}", e);
            }
        }

        // Periodic maintenance
        if let Err(e) = periodic_maintenance(&dir) {
            eprintln!("Maintenance error: {}", e);
        }
    }

    // Cleanup
    let _ = fs::remove_file(&socket);
    ServiceState::remove(&dir)?;

    Ok(())
}

#[cfg(not(unix))]
pub fn run_daemon(_dir: &Path, _socket_path: &str) -> Result<()> {
    anyhow::bail!("Daemon is only supported on Unix systems")
}

/// Handle a single IPC connection
#[cfg(unix)]
fn handle_connection(dir: &Path, stream: UnixStream, running: &mut bool) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    // Clone stream for writing
    let mut write_stream = stream.try_clone()
        .context("Failed to clone stream for writing")?;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                let response = IpcResponse::error(&format!("Read error: {}", e));
                let _ = write_response(&mut write_stream, &response);
                return Ok(());
            }
        };

        if line.is_empty() {
            continue;
        }

        let request: IpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let response = IpcResponse::error(&format!("Invalid request: {}", e));
                write_response(&mut write_stream, &response)?;
                continue;
            }
        };

        let response = handle_request(dir, request, running);
        write_response(&mut write_stream, &response)?;

        // Check if we should stop
        if !*running {
            break;
        }
    }

    Ok(())
}

#[cfg(unix)]
fn write_response(stream: &mut UnixStream, response: &IpcResponse) -> Result<()> {
    let json = serde_json::to_string(response)?;
    writeln!(stream, "{}", json)?;
    stream.flush()?;
    Ok(())
}

/// Handle an IPC request
fn handle_request(dir: &Path, request: IpcRequest, running: &mut bool) -> IpcResponse {
    match request {
        IpcRequest::Spawn { task_id, executor, timeout } => {
            handle_spawn(dir, &task_id, &executor, timeout.as_deref())
        }
        IpcRequest::Agents => handle_agents(dir),
        IpcRequest::Kill { agent_id, force } => handle_kill(dir, &agent_id, force),
        IpcRequest::Heartbeat { agent_id } => handle_heartbeat(dir, &agent_id),
        IpcRequest::Status => handle_status(dir),
        IpcRequest::Shutdown { force } => {
            *running = false;
            handle_shutdown(dir, force)
        }
    }
}

/// Handle spawn request
fn handle_spawn(dir: &Path, task_id: &str, executor: &str, timeout: Option<&str>) -> IpcResponse {
    // Use the spawn command implementation
    match crate::commands::spawn::spawn_agent(dir, task_id, executor, timeout) {
        Ok((agent_id, pid)) => IpcResponse::success(serde_json::json!({
            "agent_id": agent_id,
            "pid": pid,
            "task_id": task_id,
            "executor": executor,
        })),
        Err(e) => IpcResponse::error(&e.to_string()),
    }
}

/// Handle agents list request
fn handle_agents(dir: &Path) -> IpcResponse {
    match AgentRegistry::load(dir) {
        Ok(registry) => {
            let agents: Vec<_> = registry.list_agents().iter().map(|a| {
                serde_json::json!({
                    "id": a.id,
                    "task_id": a.task_id,
                    "executor": a.executor,
                    "pid": a.pid,
                    "status": format!("{:?}", a.status).to_lowercase(),
                    "uptime": a.uptime_human(),
                    "started_at": a.started_at,
                    "last_heartbeat": a.last_heartbeat,
                })
            }).collect();
            IpcResponse::success(serde_json::json!({ "agents": agents }))
        }
        Err(e) => IpcResponse::error(&e.to_string()),
    }
}

/// Handle kill request
fn handle_kill(dir: &Path, agent_id: &str, force: bool) -> IpcResponse {
    match crate::commands::kill::run(dir, agent_id, force, true) {
        Ok(()) => IpcResponse::success(serde_json::json!({
            "killed": agent_id,
            "force": force,
        })),
        Err(e) => IpcResponse::error(&e.to_string()),
    }
}

/// Handle heartbeat request
fn handle_heartbeat(dir: &Path, agent_id: &str) -> IpcResponse {
    match AgentRegistry::load_locked(dir) {
        Ok(mut locked) => {
            if locked.heartbeat(agent_id) {
                if let Err(e) = locked.save() {
                    return IpcResponse::error(&e.to_string());
                }
                IpcResponse::success(serde_json::json!({
                    "agent_id": agent_id,
                    "heartbeat": "recorded",
                }))
            } else {
                IpcResponse::error(&format!("Agent '{}' not found", agent_id))
            }
        }
        Err(e) => IpcResponse::error(&e.to_string()),
    }
}

/// Handle status request
fn handle_status(dir: &Path) -> IpcResponse {
    let state = match ServiceState::load(dir) {
        Ok(Some(s)) => s,
        Ok(None) => return IpcResponse::error("No service state found"),
        Err(e) => return IpcResponse::error(&e.to_string()),
    };

    let registry = AgentRegistry::load(dir).unwrap_or_default();
    let alive_count = registry.active_count();
    let idle_count = registry.idle_count();

    IpcResponse::success(serde_json::json!({
        "status": "running",
        "pid": state.pid,
        "socket": state.socket_path,
        "started_at": state.started_at,
        "agents": {
            "alive": alive_count,
            "idle": idle_count,
            "total": registry.agents.len(),
        }
    }))
}

/// Handle shutdown request
fn handle_shutdown(dir: &Path, force: bool) -> IpcResponse {
    if force {
        // Kill all agents
        if let Err(e) = crate::commands::kill::run_all(dir, true, true) {
            eprintln!("Error killing agents: {}", e);
        }
    }

    IpcResponse::success(serde_json::json!({
        "status": "shutting_down",
        "force": force,
    }))
}

/// Periodic maintenance tasks
fn periodic_maintenance(dir: &Path) -> Result<()> {
    // Check for dead agents every iteration (daemon runs with 100ms sleep)
    // Only actually do maintenance every ~60 seconds
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // Run maintenance every ~600 iterations (60 seconds at 100ms sleep)
    if count % 600 != 0 {
        return Ok(());
    }

    // Mark dead agents (default 2 minute timeout)
    let mut locked = AgentRegistry::load_locked(dir)?;
    let dead_ids = locked.mark_dead_agents(120);
    if !dead_ids.is_empty() {
        locked.save()?;
        for id in &dead_ids {
            eprintln!("Marked agent {} as dead (no heartbeat)", id);
        }
    }

    Ok(())
}

/// Stop the service daemon
#[cfg(unix)]
pub fn run_stop(dir: &Path, force: bool, json: bool) -> Result<()> {
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
    if socket.exists() {
        if let Ok(mut stream) = UnixStream::connect(&socket) {
            let request = IpcRequest::Shutdown { force };
            let json_req = serde_json::to_string(&request)?;
            let _ = writeln!(stream, "{}", json_req);
            let _ = stream.flush();
            // Give it a moment to process
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    // If process is still running, kill it
    if is_process_running(state.pid) {
        if force {
            kill_process_force(state.pid)?;
        } else {
            kill_process_graceful(state.pid)?;
        }
    }

    // Clean up
    if socket.exists() {
        let _ = fs::remove_file(&socket);
    }
    ServiceState::remove(dir)?;

    if json {
        let output = serde_json::json!({
            "status": "stopped",
            "pid": state.pid,
            "force": force,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Service stopped (PID {})", state.pid);
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_stop(_dir: &Path, _force: bool, _json: bool) -> Result<()> {
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

    let running = is_process_running(state.pid);

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

    // Get agent summary
    let registry = AgentRegistry::load(dir).unwrap_or_default();
    let alive_count = registry.active_count();
    let idle_count = registry.idle_count();

    // Calculate uptime
    let uptime = chrono::DateTime::parse_from_rfc3339(&state.started_at)
        .map(|started| {
            let now = chrono::Utc::now();
            let duration = now.signed_duration_since(started);
            format_duration(duration.num_seconds())
        })
        .unwrap_or_else(|_| "unknown".to_string());

    if json {
        let output = serde_json::json!({
            "status": "running",
            "pid": state.pid,
            "socket": state.socket_path,
            "started_at": state.started_at,
            "uptime": uptime,
            "agents": {
                "alive": alive_count,
                "idle": idle_count,
                "total": registry.agents.len(),
            }
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("Service: running (PID {})", state.pid);
        println!("Socket: {}", state.socket_path);
        println!("Uptime: {}", uptime);
        println!("Agents: {} alive, {} idle", alive_count, idle_count);
    }

    Ok(())
}

#[cfg(not(unix))]
pub fn run_status(_dir: &Path, _json: bool) -> Result<()> {
    anyhow::bail!("Service daemon is only supported on Unix systems")
}

/// Format a duration in seconds to human-readable string
fn format_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        format!("{}h {}m", hours, mins)
    } else {
        let days = secs / 86400;
        let hours = (secs % 86400) / 3600;
        format!("{}d {}h", days, hours)
    }
}

/// Check if a process is running
#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn is_process_running(_pid: u32) -> bool {
    true
}

/// Send SIGTERM, wait, then SIGKILL
#[cfg(unix)]
fn kill_process_graceful(pid: u32) -> Result<()> {
    let pid_i32 = pid as i32;

    if unsafe { libc::kill(pid_i32, 0) } != 0 {
        return Ok(());
    }

    if unsafe { libc::kill(pid_i32, libc::SIGTERM) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(err).context(format!("Failed to send SIGTERM to PID {}", pid));
    }

    for _ in 0..5 {
        std::thread::sleep(Duration::from_secs(1));
        if unsafe { libc::kill(pid_i32, 0) } != 0 {
            return Ok(());
        }
    }

    if unsafe { libc::kill(pid_i32, libc::SIGKILL) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(err).context(format!("Failed to send SIGKILL to PID {}", pid));
    }

    Ok(())
}

#[cfg(not(unix))]
fn kill_process_graceful(_pid: u32) -> Result<()> {
    anyhow::bail!("Process killing is only supported on Unix systems")
}

/// Send SIGKILL immediately
#[cfg(unix)]
fn kill_process_force(pid: u32) -> Result<()> {
    let pid_i32 = pid as i32;

    if unsafe { libc::kill(pid_i32, 0) } != 0 {
        return Ok(());
    }

    if unsafe { libc::kill(pid_i32, libc::SIGKILL) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(err).context(format!("Failed to send SIGKILL to PID {}", pid));
    }

    Ok(())
}

#[cfg(not(unix))]
fn kill_process_force(_pid: u32) -> Result<()> {
    anyhow::bail!("Process killing is only supported on Unix systems")
}

/// Send an IPC request to the running service
#[cfg(unix)]
pub fn send_request(dir: &Path, request: IpcRequest) -> Result<IpcResponse> {
    let state = ServiceState::load(dir)?
        .ok_or_else(|| anyhow::anyhow!("Service not running"))?;

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
            let response: IpcResponse = serde_json::from_str(&line)
                .context("Failed to parse response")?;
            return Ok(response);
        }
    }

    anyhow::bail!("No response from service")
}

#[cfg(not(unix))]
pub fn send_request(_dir: &Path, _request: IpcRequest) -> Result<IpcResponse> {
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
        assert!(socket.to_string_lossy().starts_with("/tmp/wg-"));
        assert!(socket.to_string_lossy().ends_with(".sock"));
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
    fn test_ipc_request_serialization() {
        let req = IpcRequest::Spawn {
            task_id: "task-1".to_string(),
            executor: "claude".to_string(),
            timeout: Some("30m".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"cmd\":\"spawn\""));
        assert!(json.contains("\"task_id\":\"task-1\""));

        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            IpcRequest::Spawn { task_id, executor, timeout } => {
                assert_eq!(task_id, "task-1");
                assert_eq!(executor, "claude");
                assert_eq!(timeout, Some("30m".to_string()));
            }
            _ => panic!("Wrong request type"),
        }
    }

    #[test]
    fn test_ipc_response_success() {
        let resp = IpcResponse::success(serde_json::json!({"agent_id": "agent-1"}));
        assert!(resp.ok);
        assert!(resp.error.is_none());
        assert!(resp.data.is_some());
    }

    #[test]
    fn test_ipc_response_error() {
        let resp = IpcResponse::error("Something went wrong");
        assert!(!resp.ok);
        assert_eq!(resp.error, Some("Something went wrong".to_string()));
        assert!(resp.data.is_none());
    }

    #[test]
    fn test_format_duration() {
        assert_eq!(format_duration(30), "30s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3661), "1h 1m");
        assert_eq!(format_duration(90000), "1d 1h");
    }

    #[test]
    fn test_is_process_running() {
        // Current process should be running
        #[cfg(unix)]
        {
            let pid = std::process::id();
            assert!(is_process_running(pid));
        }

        // Non-existent process
        #[cfg(unix)]
        assert!(!is_process_running(999999999));
    }

    #[test]
    fn test_status_not_running() {
        let temp_dir = TempDir::new().unwrap();
        // No state file, should report not running
        let result = run_status(temp_dir.path(), false);
        assert!(result.is_ok());
    }
}
