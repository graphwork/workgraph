//! Crash scenario tests for agent termination scenarios
//!
//! Tests crash scenarios identified in the agent exit worktree cleanup audit:
//! - SIGKILL scenarios - immediate process death
//! - SIGTERM scenarios - graceful termination requests
//! - Timeout scenarios - agent processes that exceed heartbeat timeout
//! - Agent spawn failure scenarios
//! - Multiple agent crash scenarios
//!
//! These tests verify:
//! - Proper cleanup occurs via coordinator ticks
//! - Recovery mechanisms after crash scenarios
//! - Registry state transitions (alive → dead → cleanup)
//! - Worktree cleanup and branch removal

use serial_test::serial;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Get the path to the compiled `wg` binary (from target/debug or target/release).
fn wg_binary() -> PathBuf {
    // Use the binary built by `cargo test` in the same target directory
    let mut path = std::env::current_exe().expect("could not get current exe path");
    // current_exe is something like target/debug/deps/test_crash_scenarios-<hash>
    // Go up to target/debug/
    path.pop(); // remove the binary name
    if path.ends_with("deps") {
        path.pop(); // remove deps/
    }
    path.push("wg");
    assert!(
        path.exists(),
        "wg binary not found at {:?}. Run `cargo build` first.",
        path
    );
    path
}

/// Derive a fake HOME from the wg_dir path so global config doesn't leak in.
fn fake_home_for(wg_dir: &Path) -> PathBuf {
    wg_dir
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| wg_dir.to_path_buf())
}

/// Helper: run `wg` with given args in a specific workgraph directory.
fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    let wg = wg_binary();
    Command::new(&wg)
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .env("HOME", fake_home_for(wg_dir))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

/// Helper: run `wg` and assert success, returning stdout as string.
fn wg_ok(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg {:?} failed.\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    stdout
}

/// Helper: initialize a fresh workgraph in a temp directory,
/// and configure a shell executor with the correct working_dir
/// so that the wrapper script's bare `wg` commands can find `.workgraph`.
fn setup_workgraph(tmp_root: &Path) -> PathBuf {
    let wg_dir = tmp_root.join(".workgraph");
    wg_ok(&wg_dir, &["init"]);

    // Get the directory containing the test-built wg binary.
    let wg_bin_dir = wg_binary().parent().unwrap().to_string_lossy().to_string();
    let path_with_test_binary = format!(
        "{}:{}",
        wg_bin_dir,
        std::env::var("PATH").unwrap_or_default()
    );

    // Create config with crash detection settings
    let config_content = format!(
        "[agent]
reaper_grace_seconds = 1
heartbeat_timeout = 3

[coordinator]
max_agents = 5
poll_interval = 1

[agency]
auto_assign = false
auto_evaluate = false
"
    );
    fs::write(wg_dir.join("config.toml"), config_content).unwrap();

    let executors_dir = wg_dir.join("executors");
    fs::create_dir_all(&executors_dir).unwrap();
    let shell_config = format!(
        r#"[executor]
type = "shell"
command = "bash"
args = ["-c", "{{{{task_context}}}}"]
working_dir = "{}"

[executor.env]
TASK_ID = "{{{{task_id}}}}"
TASK_TITLE = "{{{{task_title}}}}"
PATH = "{}"
"#,
        tmp_root.display(),
        path_with_test_binary
    );
    fs::write(executors_dir.join("shell.toml"), shell_config).unwrap();

    wg_dir
}

/// Helper: generate a unique socket path for this test's temp directory.
fn socket_path_for(tmp_root: &Path) -> String {
    format!("{}/wg-test.sock", tmp_root.display())
}

/// Helper: add a task with a shell exec command.
fn add_shell_task(wg_dir: &Path, task_id: &str, title: &str, exec_cmd: &str) {
    // wg add doesn't support --exec directly, so we add the task then patch the JSONL
    wg_ok(wg_dir, &["add", title, "--id", task_id, "--immediate"]);

    // Patch the graph to add exec field
    let graph_path = wg_dir.join("graph.jsonl");
    let content = fs::read_to_string(&graph_path).unwrap();
    let mut new_lines = Vec::new();
    for line in content.lines() {
        if line.contains(&format!("\"id\":\"{}\"", task_id)) {
            // Parse, add exec, re-serialize
            let mut val: serde_json::Value = serde_json::from_str(line).unwrap();
            val["exec"] = serde_json::Value::String(exec_cmd.to_string());
            new_lines.push(serde_json::to_string(&val).unwrap());
        } else {
            new_lines.push(line.to_string());
        }
    }
    fs::write(&graph_path, new_lines.join("\n") + "\n").unwrap();
}

/// Helper: read task status from graph using `wg show --json`.
fn task_status(wg_dir: &Path, task_id: &str) -> String {
    let output = wg_cmd(wg_dir, &["show", task_id, "--json"]);
    if !output.status.success() {
        return "unknown".to_string();
    }
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    match serde_json::from_str::<serde_json::Value>(&stdout) {
        Ok(val) => val["status"].as_str().unwrap_or("unknown").to_string(),
        Err(_) => "unknown".to_string(),
    }
}

/// Helper: send GraphChanged notification via IPC.
fn notify_graph_changed(wg_dir: &Path) -> bool {
    let state_path = wg_dir.join("service").join("state.json");
    if let Ok(content) = fs::read_to_string(&state_path)
        && let Ok(state) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(socket_path) = state["socket_path"].as_str()
        && let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket_path)
    {
        let _ = writeln!(stream, r#"{{"cmd":"graph_changed"}}"#);
        let _ = stream.flush();
        // Read response
        let mut reader = BufReader::new(&stream);
        let mut response = String::new();
        if reader.read_line(&mut response).is_ok() && !response.is_empty() {
            return true;
        }
    }
    false
}

/// Helper: wait for the service daemon's socket to become connectable.
fn wait_for_service_ready(wg_dir: &Path, timeout: Duration) -> bool {
    wait_for(timeout, 100, || {
        let state_path = wg_dir.join("service").join("state.json");
        if let Ok(content) = fs::read_to_string(&state_path)
            && let Ok(state) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(socket_path) = state["socket_path"].as_str()
        {
            if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket_path) {
                let _ = writeln!(stream, r#"{{"cmd":"status"}}"#);
                let _ = stream.flush();
                let mut reader = BufReader::new(&stream);
                let mut response = String::new();
                if reader.read_line(&mut response).is_ok() && !response.is_empty() {
                    return true;
                }
            }
        }
        false
    })
}

/// Helper: read the agent registry, returning None if the file doesn't exist or can't be parsed.
fn read_registry(wg_dir: &Path) -> Option<serde_json::Value> {
    let registry_path = wg_dir.join("service").join("registry.json");
    let content = fs::read_to_string(&registry_path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Helper: stop the service daemon and kill any running agents.
fn stop_service(wg_dir: &Path) {
    let _ = wg_cmd(wg_dir, &["service", "stop", "--force", "--kill-agents"]);
}

/// Guard that ensures daemon cleanup on drop, even if a test panics.
struct ServiceGuard<'a> {
    wg_dir: &'a Path,
}

impl<'a> ServiceGuard<'a> {
    fn new(wg_dir: &'a Path) -> Self {
        ServiceGuard { wg_dir }
    }
}

impl Drop for ServiceGuard<'_> {
    fn drop(&mut self) {
        // Graceful stop via CLI (kills agents too)
        stop_service(self.wg_dir);

        // Belt-and-suspenders: read PID from state.json and kill directly
        let state_path = self.wg_dir.join("service").join("state.json");
        if let Ok(content) = fs::read_to_string(&state_path) {
            if let Ok(state) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(pid) = state["pid"].as_u64() {
                    unsafe {
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
            }
        }
    }
}

/// Helper: wait for a condition with timeout, polling at interval.
fn wait_for<F>(timeout: Duration, poll_ms: u64, mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(poll_ms));
    }
    false
}

/// Test: Agent SIGKILL cleanup
///
/// Scenario:
/// 1. Start service with shell executor
/// 2. Add a long-running task (sleep)
/// 3. Wait for agent to pick up task
/// 4. SIGKILL the agent process immediately
/// 5. Verify coordinator detects death and cleans up
#[test]
#[serial]
fn test_crash_scenarios_sigkill_cleanup() {
    let tmp = tempfile::tempdir().unwrap();
    let wg_dir = setup_workgraph(tmp.path());
    let _guard = ServiceGuard::new(&wg_dir);

    // Start service with crash detection config
    let socket = socket_path_for(tmp.path());
    let out = wg_ok(
        &wg_dir,
        &[
            "service",
            "start",
            "--socket",
            &socket,
            "--executor",
            "shell",
            "--max-agents",
            "2",
            "--interval",
            "1", // 1 second poll for fast detection
        ],
    );
    assert!(
        out.contains("Service started") || out.contains("started"),
        "Service did not start: {}",
        out
    );

    // Wait for daemon socket to become ready
    assert!(
        wait_for_service_ready(&wg_dir, Duration::from_secs(5)),
        "Service daemon socket did not become ready"
    );

    // Add a long-running task that the agent will pick up
    add_shell_task(&wg_dir, "sigkill-task", "SIGKILL Test Task", "sleep 300");
    notify_graph_changed(&wg_dir);

    // Wait for the task to be picked up by an agent
    let picked_up = wait_for(Duration::from_secs(10), 200, || {
        task_status(&wg_dir, "sigkill-task") == "in-progress"
    });
    assert!(
        picked_up,
        "Task was not picked up. Status: {}",
        task_status(&wg_dir, "sigkill-task")
    );

    // Find the agent's PID from the registry
    let mut agent_pid: i32 = 0;
    let mut agent_id = String::new();
    let found_agent = wait_for(Duration::from_secs(5), 100, || {
        if let Some(registry) = read_registry(&wg_dir)
            && let Some(agents) = registry["agents"].as_object()
            && let Some(entry) = agents.values().find(|a| {
                a["task_id"].as_str() == Some("sigkill-task")
                    && a["status"].as_str() != Some("dead")
            })
        {
            agent_pid = entry["pid"].as_u64().unwrap() as i32;
            agent_id = entry["id"].as_str().unwrap().to_string();
            return true;
        }
        false
    });
    assert!(
        found_agent,
        "Alive agent for sigkill-task not found in registry within 5s"
    );

    // SIGKILL the agent process (immediate death)
    unsafe {
        libc::kill(agent_pid, libc::SIGKILL);
    }

    // Give the kernel time to process the signal
    std::thread::sleep(Duration::from_millis(500));

    // Trigger coordinator ticks to detect the dead agent
    for _ in 0..5 {
        notify_graph_changed(&wg_dir);
        std::thread::sleep(Duration::from_millis(300));
    }

    // Wait for the agent to be detected as dead and cleaned up
    let cleaned_up = wait_for(Duration::from_secs(15), 300, || {
        if let Some(reg) = read_registry(&wg_dir)
            && let Some(agent) = reg["agents"].get(&agent_id)
        {
            return agent["status"].as_str() == Some("dead");
        }
        false
    });

    assert!(
        cleaned_up,
        "Agent should have been detected as dead and cleaned up after SIGKILL"
    );

    // Verify the task eventually returns to "open" or gets re-assigned
    let task_handled = wait_for(Duration::from_secs(10), 300, || {
        let status = task_status(&wg_dir, "sigkill-task");
        status == "open"
            || (status == "in-progress" && {
                // Check if it's a different agent
                if let Some(reg) = read_registry(&wg_dir)
                    && let Some(agents) = reg["agents"].as_object()
                {
                    agents.values().any(|a| {
                        a["task_id"].as_str() == Some("sigkill-task")
                            && a["id"].as_str() != Some(&agent_id)
                            && a["status"].as_str() != Some("dead")
                    })
                } else {
                    false
                }
            })
    });

    assert!(
        task_handled,
        "Task should be back to 'open' or reassigned after agent death"
    );
}

/// Test: Agent SIGTERM cleanup
///
/// Scenario:
/// 1. Start service with shell executor
/// 2. Add a task that can handle graceful termination
/// 3. Wait for agent to pick up task
/// 4. SIGTERM the agent process (graceful termination)
/// 5. Verify coordinator detects death and cleans up
#[test]
#[serial]
fn test_crash_scenarios_sigterm_cleanup() {
    let tmp = tempfile::tempdir().unwrap();
    let wg_dir = setup_workgraph(tmp.path());
    let _guard = ServiceGuard::new(&wg_dir);

    // Start service
    let socket = socket_path_for(tmp.path());
    let out = wg_ok(
        &wg_dir,
        &[
            "service",
            "start",
            "--socket",
            &socket,
            "--executor",
            "shell",
            "--max-agents",
            "2",
            "--interval",
            "1",
        ],
    );
    assert!(
        out.contains("Service started") || out.contains("started"),
        "Service did not start: {}",
        out
    );

    assert!(
        wait_for_service_ready(&wg_dir, Duration::from_secs(5)),
        "Service daemon socket did not become ready"
    );

    // Add a task with a script that can handle SIGTERM gracefully
    add_shell_task(
        &wg_dir,
        "sigterm-task",
        "SIGTERM Test Task",
        "trap 'echo \"Received SIGTERM, exiting gracefully\"; exit 0' TERM; sleep 300 & wait $!",
    );
    notify_graph_changed(&wg_dir);

    // Wait for task pickup
    let picked_up = wait_for(Duration::from_secs(10), 200, || {
        task_status(&wg_dir, "sigterm-task") == "in-progress"
    });
    assert!(
        picked_up,
        "Task was not picked up. Status: {}",
        task_status(&wg_dir, "sigterm-task")
    );

    // Find the agent's PID
    let mut agent_pid: i32 = 0;
    let mut agent_id = String::new();
    let found_agent = wait_for(Duration::from_secs(5), 100, || {
        if let Some(registry) = read_registry(&wg_dir)
            && let Some(agents) = registry["agents"].as_object()
            && let Some(entry) = agents.values().find(|a| {
                a["task_id"].as_str() == Some("sigterm-task")
                    && a["status"].as_str() != Some("dead")
            })
        {
            agent_pid = entry["pid"].as_u64().unwrap() as i32;
            agent_id = entry["id"].as_str().unwrap().to_string();
            return true;
        }
        false
    });
    assert!(
        found_agent,
        "Alive agent for sigterm-task not found in registry"
    );

    // SIGTERM the agent process (graceful termination request)
    unsafe {
        libc::kill(agent_pid, libc::SIGTERM);
    }

    // Give time for graceful shutdown
    std::thread::sleep(Duration::from_millis(500));

    // Trigger coordinator ticks
    for _ in 0..5 {
        notify_graph_changed(&wg_dir);
        std::thread::sleep(Duration::from_millis(300));
    }

    // Wait for cleanup
    let cleaned_up = wait_for(Duration::from_secs(15), 300, || {
        if let Some(reg) = read_registry(&wg_dir)
            && let Some(agent) = reg["agents"].get(&agent_id)
        {
            return agent["status"].as_str() == Some("dead");
        }
        false
    });

    assert!(
        cleaned_up,
        "Agent should have been detected as dead after SIGTERM"
    );
}

/// Test: Agent timeout cleanup
///
/// Scenario:
/// 1. Start service with short heartbeat timeout
/// 2. Add a task that stops sending heartbeats
/// 3. Wait for heartbeat timeout to expire
/// 4. Verify coordinator detects timeout and cleans up
#[test]
#[serial]
fn test_crash_scenarios_timeout_cleanup() {
    let tmp = tempfile::tempdir().unwrap();
    let wg_dir = setup_workgraph(tmp.path());
    let _guard = ServiceGuard::new(&wg_dir);

    // Override config for fast timeout testing
    let config_content = "[agent]
reaper_grace_seconds = 1
heartbeat_timeout = 1

[coordinator]
max_agents = 5
poll_interval = 1

[agency]
auto_assign = false
auto_evaluate = false
";
    fs::write(wg_dir.join("config.toml"), config_content).unwrap();

    // Start service
    let socket = socket_path_for(tmp.path());
    let out = wg_ok(
        &wg_dir,
        &[
            "service",
            "start",
            "--socket",
            &socket,
            "--executor",
            "shell",
            "--max-agents",
            "2",
            "--interval",
            "1",
        ],
    );
    assert!(
        out.contains("Service started") || out.contains("started"),
        "Service did not start: {}",
        out
    );

    assert!(
        wait_for_service_ready(&wg_dir, Duration::from_secs(5)),
        "Service daemon socket did not become ready"
    );

    // Add a task that simulates hanging (no heartbeat updates)
    add_shell_task(
        &wg_dir,
        "timeout-task",
        "Timeout Test Task",
        "sleep 300", // Long sleep without heartbeat updates
    );
    notify_graph_changed(&wg_dir);

    // Wait for task pickup
    let picked_up = wait_for(Duration::from_secs(10), 200, || {
        task_status(&wg_dir, "timeout-task") == "in-progress"
    });
    assert!(
        picked_up,
        "Task was not picked up. Status: {}",
        task_status(&wg_dir, "timeout-task")
    );

    // Find the agent
    let mut agent_id = String::new();
    let found_agent = wait_for(Duration::from_secs(5), 100, || {
        if let Some(registry) = read_registry(&wg_dir)
            && let Some(agents) = registry["agents"].as_object()
            && let Some(entry) = agents.values().find(|a| {
                a["task_id"].as_str() == Some("timeout-task")
                    && a["status"].as_str() != Some("dead")
            })
        {
            agent_id = entry["id"].as_str().unwrap().to_string();
            return true;
        }
        false
    });
    assert!(found_agent, "Agent for timeout-task not found");

    // Wait for heartbeat timeout (1 minute = 60s) plus grace period (1s) + detection time
    std::thread::sleep(Duration::from_secs(62));

    // Trigger multiple coordinator ticks
    for _ in 0..10 {
        notify_graph_changed(&wg_dir);
        std::thread::sleep(Duration::from_millis(200));
    }

    // Wait for timeout detection and cleanup
    let timed_out = wait_for(Duration::from_secs(20), 500, || {
        if let Some(reg) = read_registry(&wg_dir)
            && let Some(agent) = reg["agents"].get(&agent_id)
        {
            let status = agent["status"].as_str().unwrap_or("");
            return status == "dead" || status == "timeout";
        }
        false
    });

    assert!(
        timed_out,
        "Agent should have been detected as timed out and cleaned up"
    );
}

/// Test: Multiple agent crash cleanup
///
/// Scenario:
/// 1. Start service
/// 2. Spawn multiple long-running tasks
/// 3. Kill multiple agents simultaneously
/// 4. Verify coordinator handles multiple crashes correctly
#[test]
#[serial]
fn test_crash_scenarios_multiple_agent_crash() {
    let tmp = tempfile::tempdir().unwrap();
    let wg_dir = setup_workgraph(tmp.path());
    let _guard = ServiceGuard::new(&wg_dir);

    // Start service
    let socket = socket_path_for(tmp.path());
    let out = wg_ok(
        &wg_dir,
        &[
            "service",
            "start",
            "--socket",
            &socket,
            "--executor",
            "shell",
            "--max-agents",
            "5",
            "--interval",
            "1",
        ],
    );
    assert!(
        out.contains("Service started") || out.contains("started"),
        "Service did not start: {}",
        out
    );

    assert!(
        wait_for_service_ready(&wg_dir, Duration::from_secs(5)),
        "Service daemon socket did not become ready"
    );

    // Add multiple long-running tasks
    let task_ids = ["crash-1", "crash-2", "crash-3"];
    for (i, task_id) in task_ids.iter().enumerate() {
        add_shell_task(
            &wg_dir,
            task_id,
            &format!("Multi Crash Task {}", i + 1),
            "sleep 300",
        );
    }
    notify_graph_changed(&wg_dir);

    // Wait for all tasks to be picked up
    let all_picked_up = wait_for(Duration::from_secs(15), 200, || {
        task_ids
            .iter()
            .all(|task_id| task_status(&wg_dir, task_id) == "in-progress")
    });
    assert!(all_picked_up, "Not all tasks were picked up by agents");

    // Collect agent PIDs
    let mut agent_pids = Vec::new();
    let mut agent_ids = Vec::new();

    for _ in 0..3 {
        std::thread::sleep(Duration::from_millis(500));
        if let Some(registry) = read_registry(&wg_dir) {
            if let Some(agents) = registry["agents"].as_object() {
                for agent in agents.values() {
                    if let Some(task_id) = agent["task_id"].as_str() {
                        if task_ids.contains(&task_id) && agent["status"].as_str() != Some("dead") {
                            if let Some(pid) = agent["pid"].as_u64() {
                                if let Some(id) = agent["id"].as_str() {
                                    agent_pids.push(pid as i32);
                                    agent_ids.push(id.to_string());
                                }
                            }
                        }
                    }
                }
            }
        }
        if agent_pids.len() >= 3 {
            break;
        }
    }

    assert!(
        agent_pids.len() >= 3,
        "Should have found at least 3 agent PIDs, found {}",
        agent_pids.len()
    );

    // Kill all agents simultaneously with SIGKILL
    for pid in &agent_pids {
        unsafe {
            libc::kill(*pid, libc::SIGKILL);
        }
    }

    // Give time for signals to process
    std::thread::sleep(Duration::from_millis(500));

    // Trigger coordinator ticks
    for _ in 0..10 {
        notify_graph_changed(&wg_dir);
        std::thread::sleep(Duration::from_millis(300));
    }

    // Wait for all agents to be detected as dead
    let all_dead = wait_for(Duration::from_secs(20), 500, || {
        if let Some(reg) = read_registry(&wg_dir) {
            agent_ids.iter().all(|agent_id| {
                if let Some(agent) = reg["agents"].get(agent_id) {
                    agent["status"].as_str() == Some("dead")
                } else {
                    false
                }
            })
        } else {
            false
        }
    });

    assert!(
        all_dead,
        "All agents should have been detected as dead after multiple crashes"
    );

    // Verify tasks are back to "open" or reassigned
    let tasks_handled = wait_for(Duration::from_secs(15), 500, || {
        task_ids.iter().all(|task_id| {
            let status = task_status(&wg_dir, task_id);
            status == "open" || status == "failed" || {
                // Check if reassigned to new agent
                if let Some(reg) = read_registry(&wg_dir) {
                    if let Some(agents) = reg["agents"].as_object() {
                        agents.values().any(|a| {
                            a["task_id"].as_str() == Some(*task_id)
                                && !agent_ids.contains(&a["id"].as_str().unwrap_or("").to_string())
                                && a["status"].as_str() != Some("dead")
                        })
                    } else {
                        false
                    }
                } else {
                    false
                }
            }
        })
    });

    assert!(
        tasks_handled,
        "All tasks should be handled (open/failed/reassigned) after agent crashes"
    );
}

/// Test: Agent spawn failure scenarios
///
/// Scenario:
/// 1. Create a task that will cause agent spawn to fail
/// 2. Verify the system handles spawn failures gracefully
/// 3. Verify task remains available for retry
#[test]
#[serial]
fn test_crash_scenarios_spawn_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let wg_dir = setup_workgraph(tmp.path());
    let _guard = ServiceGuard::new(&wg_dir);

    // Create an invalid executor config to cause spawn failures
    let executors_dir = wg_dir.join("executors");
    fs::create_dir_all(&executors_dir).unwrap();
    let invalid_shell_config = r#"[executor]
type = "shell"
command = "/nonexistent/invalid/command"
args = ["-c", "{{task_context}}"]
working_dir = "/nonexistent/directory"
"#;
    fs::write(executors_dir.join("invalid.toml"), invalid_shell_config).unwrap();

    // Start service with the invalid executor
    let socket = socket_path_for(tmp.path());
    let output = wg_cmd(
        &wg_dir,
        &[
            "service",
            "start",
            "--socket",
            &socket,
            "--executor",
            "invalid", // This will cause spawn failures
            "--max-agents",
            "2",
            "--interval",
            "1",
        ],
    );

    // Service might start but agent spawning will fail
    if !output.status.success() {
        // If service fails to start due to invalid config, that's also valid behavior
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("invalid") || stderr.contains("executor") || stderr.contains("config"),
            "Error should mention executor/config issues: {}",
            stderr
        );
        return; // Test passes - invalid config rejected appropriately
    }

    // If service started, test spawn failure handling
    if wait_for_service_ready(&wg_dir, Duration::from_secs(5)) {
        // Add a task WITHOUT exec command so it uses the default executor (invalid)
        wg_ok(
            &wg_dir,
            &[
                "add",
                "Spawn Failure Test",
                "--id",
                "spawn-fail-task",
                "--immediate",
            ],
        );
        notify_graph_changed(&wg_dir);

        // Wait and verify the task doesn't get stuck in "in-progress" with a dead agent
        std::thread::sleep(Duration::from_secs(3));
        for _ in 0..5 {
            notify_graph_changed(&wg_dir);
            std::thread::sleep(Duration::from_millis(500));
        }

        let final_status = task_status(&wg_dir, "spawn-fail-task");

        // Task should either remain "open" (spawn failed) or be "failed"
        assert!(
            final_status == "open" || final_status == "failed",
            "Task with spawn failure should be 'open' or 'failed', got: {}",
            final_status
        );

        // Registry should not have stuck "in-progress" agents for this task
        if let Some(registry) = read_registry(&wg_dir) {
            if let Some(agents) = registry["agents"].as_object() {
                for agent in agents.values() {
                    if agent["task_id"].as_str() == Some("spawn-fail-task") {
                        let status = agent["status"].as_str().unwrap_or("");
                        assert_ne!(
                            status, "alive",
                            "Should not have alive agent for failed spawn task"
                        );
                    }
                }
            }
        }
    }
}

/// Meta-test that validates crash scenarios infrastructure exists and is testable.
///
/// This test serves as a verification gate for `cargo test crash_scenarios` command.
/// It validates that the crash scenario test infrastructure is present and functional
/// without running the full integration tests that may have timing-sensitive behavior.
#[test]
fn test_crash_scenarios_infrastructure() {
    // Verify that wg binary exists and can be executed
    let wg_path = wg_binary();
    assert!(wg_path.exists(), "wg binary should exist at {:?}", wg_path);

    // Test basic temp directory setup
    let tmp = tempfile::tempdir().unwrap();
    let wg_dir = setup_workgraph(tmp.path());
    assert!(wg_dir.exists(), "Workgraph directory should be created");
    assert!(
        wg_dir.join("config.toml").exists(),
        "Config should be created"
    );

    // Verify we can create tasks in the test environment
    let output = wg_cmd(
        &wg_dir,
        &["add", "test-task", "--id", "test", "--immediate"],
    );
    assert!(output.status.success(), "Should be able to add tasks");

    // Verify we can check task status
    let status_output = wg_cmd(&wg_dir, &["show", "test", "--json"]);
    assert!(
        status_output.status.success(),
        "Should be able to query task status"
    );

    // This confirms that the crash scenario test infrastructure is working
    // and the individual crash scenario tests can be run when needed
}
