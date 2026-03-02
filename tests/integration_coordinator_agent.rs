//! Integration tests for the Phase 2 coordinator agent.
//!
//! Tests the persistent LLM-backed coordinator agent that replaces the Phase 1
//! stub response with actual LLM processing within the service daemon.
//!
//! **Mock-based tests** use a fake `claude` script to validate the plumbing
//! (message routing, context injection, crash recovery) without requiring the
//! real Claude CLI. The mock is a bash script placed on PATH before starting
//! the daemon.
//!
//! **Real E2E tests** (marked `#[ignore]`) exercise the full flow with the
//! actual Claude CLI and require it to be installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("could not get current exe path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("wg");
    assert!(
        path.exists(),
        "wg binary not found at {:?}. Run `cargo build` first.",
        path
    );
    path
}

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn wg_cmd_env(wg_dir: &Path, args: &[&str], env_vars: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(wg_binary());
    cmd.arg("--dir")
        .arg(wg_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for &(key, val) in env_vars {
        cmd.env(key, val);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

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

fn wg_ok_env(wg_dir: &Path, args: &[&str], env_vars: &[(&str, &str)]) -> String {
    let output = wg_cmd_env(wg_dir, args, env_vars);
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

/// Initialise a fresh workgraph in a temp directory and return the .workgraph path.
fn init_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);
    wg_dir
}

/// Write config.toml to enable the coordinator agent.
fn enable_coordinator_agent(wg_dir: &Path) {
    let config_path = wg_dir.join("config.toml");
    let config = "[coordinator]\ncoordinator_agent = true\n";
    fs::write(&config_path, config).unwrap();
}

/// Stop the daemon, ignoring errors (best-effort cleanup).
fn stop_daemon(wg_dir: &Path) {
    let _ = wg_cmd(wg_dir, &["service", "stop"]);
}

/// Stop daemon with custom env vars.
fn stop_daemon_env(wg_dir: &Path, env_vars: &[(&str, &str)]) {
    let _ = wg_cmd_env(wg_dir, &["service", "stop"], env_vars);
}

/// Wait for the daemon socket to appear.
fn wait_for_socket(wg_dir: &Path) {
    let socket = wg_dir.join("service").join("daemon.sock");
    let start = Instant::now();
    while !socket.exists() {
        if start.elapsed() > Duration::from_secs(10) {
            panic!("Daemon socket did not appear within 10s at {:?}", socket);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait for the coordinator agent to be spawned (look for log marker).
fn wait_for_coordinator_agent(wg_dir: &Path) {
    let log_path = wg_dir.join("service").join("daemon.log");
    let start = Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(15) {
            let log = fs::read_to_string(&log_path).unwrap_or_default();
            panic!(
                "Coordinator agent did not start within 15s.\nDaemon log:\n{}",
                log
            );
        }
        if let Ok(content) = fs::read_to_string(&log_path) {
            if content.contains("Claude CLI started")
                || content.contains("Coordinator agent spawned successfully")
            {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Read the daemon log for debugging.
fn read_daemon_log(wg_dir: &Path) -> String {
    let log_path = wg_dir.join("service").join("daemon.log");
    fs::read_to_string(&log_path).unwrap_or_else(|_| "<no log>".to_string())
}

// ---------------------------------------------------------------------------
// Mock Claude CLI
// ---------------------------------------------------------------------------

/// A mock Claude CLI that handles stream-json I/O.
///
/// Placed on PATH so the daemon's coordinator agent uses it instead of the
/// real Claude CLI. Responds with "Mock coordinator response #N" to each
/// user message.
struct MockClaude {
    _tmp: TempDir,
    dir: PathBuf,
}

impl MockClaude {
    /// Create a mock claude that echoes back responses.
    fn new() -> Self {
        Self::create(MOCK_CLAUDE_SCRIPT)
    }

    /// Create a mock claude that crashes (exit 1) when it receives a message
    /// containing "CRASH_NOW". Normal messages get normal responses.
    fn new_with_crash_trigger() -> Self {
        Self::create(MOCK_CLAUDE_CRASH_SCRIPT)
    }

    fn create(script: &str) -> Self {
        let tmp = TempDir::new().unwrap();
        let mock_path = tmp.path().join("claude");
        fs::write(&mock_path, script).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&mock_path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        MockClaude {
            dir: tmp.path().to_path_buf(),
            _tmp: tmp,
        }
    }

    /// Return PATH env var value with the mock directory prepended.
    fn path_env(&self) -> String {
        let original = std::env::var("PATH").unwrap_or_default();
        format!("{}:{}", self.dir.display(), original)
    }
}

/// Mock claude script: reads stream-json from stdin, writes mock responses.
const MOCK_CLAUDE_SCRIPT: &str = r#"#!/bin/bash
# Mock Claude CLI for coordinator agent integration testing

# Handle --version check
for arg in "$@"; do
    if [ "$arg" = "--version" ]; then
        echo "mock-claude 0.1.0"
        exit 0
    fi
done

# Stream-JSON mode: read stdin line-by-line, respond to user messages
msg_count=0
while IFS= read -r line; do
    if [[ "$line" == *'"type":"user"'* ]]; then
        msg_count=$((msg_count + 1))
        printf '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Mock coordinator response #%d"}],"stop_reason":"end_turn"}}\n' "$msg_count"
    fi
done
"#;

/// Mock claude script with a one-shot file-based crash trigger.
///
/// Checks for the file at `$MOCK_CRASH_FILE` on each message. If it exists,
/// deletes it and exits with code 1 (simulating a crash). After restart,
/// the file is gone so the mock works normally — enabling crash recovery tests
/// without entering an infinite crash loop (which happens with message-content
/// triggers because the crash recovery context replays the triggering message).
const MOCK_CLAUDE_CRASH_SCRIPT: &str = r#"#!/bin/bash
# Mock Claude CLI with one-shot file-based crash trigger

# Handle --version check
for arg in "$@"; do
    if [ "$arg" = "--version" ]; then
        echo "mock-claude 0.1.0"
        exit 0
    fi
done

# Stream-JSON mode
msg_count=0
while IFS= read -r line; do
    if [[ "$line" == *'"type":"user"'* ]]; then
        msg_count=$((msg_count + 1))
        # One-shot crash: if the trigger file exists, delete it and crash
        if [ -n "$MOCK_CRASH_FILE" ] && [ -f "$MOCK_CRASH_FILE" ]; then
            rm -f "$MOCK_CRASH_FILE"
            exit 1
        fi
        printf '{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Mock coordinator response #%d"}],"stop_reason":"end_turn"}}\n' "$msg_count"
    fi
done
"#;

// ---------------------------------------------------------------------------
// Daemon guard with mock Claude on PATH
// ---------------------------------------------------------------------------

/// Guard that starts the daemon with the coordinator agent enabled (using a
/// mock Claude CLI) and stops it on drop.
struct CoordinatorDaemonGuard<'a> {
    wg_dir: &'a Path,
    env_vars: Vec<(String, String)>,
}

impl<'a> CoordinatorDaemonGuard<'a> {
    /// Start the daemon with the coordinator agent enabled and a mock Claude on PATH.
    fn start(wg_dir: &'a Path, mock: &MockClaude) -> Self {
        Self::start_with_env(wg_dir, mock, &[])
    }

    /// Start with additional env vars (e.g., MOCK_CRASH_FILE for crash tests).
    fn start_with_env(wg_dir: &'a Path, mock: &MockClaude, extra_env: &[(&str, &str)]) -> Self {
        enable_coordinator_agent(wg_dir);

        let mut env_vars: Vec<(String, String)> = vec![("PATH".to_string(), mock.path_env())];
        for &(key, val) in extra_env {
            env_vars.push((key.to_string(), val.to_string()));
        }

        let env_refs: Vec<(&str, &str)> = env_vars
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let output = wg_cmd_env(
            wg_dir,
            &["service", "start", "--interval", "600", "--max-agents", "0"],
            &env_refs,
        );
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        assert!(
            output.status.success(),
            "service start failed.\nstdout: {}\nstderr: {}",
            stdout,
            stderr
        );

        wait_for_socket(wg_dir);
        wait_for_coordinator_agent(wg_dir);

        // Small delay to let the mock claude process fully initialize.
        std::thread::sleep(Duration::from_millis(200));

        CoordinatorDaemonGuard { wg_dir, env_vars }
    }

    fn env_refs(&self) -> Vec<(&str, &str)> {
        self.env_vars
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }

    /// Send a chat message via `wg chat` with the correct env.
    fn chat(&self, message: &str, timeout_secs: u32) -> std::process::Output {
        let timeout = timeout_secs.to_string();
        wg_cmd_env(
            self.wg_dir,
            &["chat", message, "--timeout", &timeout],
            &self.env_refs(),
        )
    }

    /// Send a chat message, assert success, return stdout.
    fn chat_ok(&self, message: &str, timeout_secs: u32) -> String {
        let output = self.chat(message, timeout_secs);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        assert!(
            output.status.success(),
            "wg chat {:?} failed.\nstdout: {}\nstderr: {}\nDaemon log:\n{}",
            message,
            stdout,
            stderr,
            read_daemon_log(self.wg_dir),
        );
        stdout
    }
}

impl Drop for CoordinatorDaemonGuard<'_> {
    fn drop(&mut self) {
        stop_daemon_env(self.wg_dir, &self.env_refs());
    }
}

// ===========================================================================
// Mock-based tests: validate plumbing without real Claude CLI
// ===========================================================================

/// Basic round-trip: send a message via `wg chat` → coordinator agent processes
/// it via the mock claude → response appears in chat output.
#[test]
fn coordinator_agent_basic_conversation() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let mock = MockClaude::new();
    let guard = CoordinatorDaemonGuard::start(&wg_dir, &mock);

    let stdout = guard.chat_ok("Hello coordinator", 15);

    // The mock responds with "Mock coordinator response #N"
    assert!(
        stdout.contains("Mock coordinator response"),
        "Expected mock coordinator response, got:\n{}\nDaemon log:\n{}",
        stdout,
        read_daemon_log(&wg_dir),
    );
}

/// Multi-turn conversation: send multiple messages in sequence, all get responses.
#[test]
fn coordinator_agent_multi_turn() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let mock = MockClaude::new();
    let guard = CoordinatorDaemonGuard::start(&wg_dir, &mock);

    // Send three messages in sequence
    let r1 = guard.chat_ok("First message", 15);
    let r2 = guard.chat_ok("Second message", 15);
    let r3 = guard.chat_ok("Third message", 15);

    // Each should get a response (mock increments counter per message)
    assert!(
        r1.contains("Mock coordinator response"),
        "First response missing mock text: {}",
        r1
    );
    assert!(
        r2.contains("Mock coordinator response"),
        "Second response missing mock text: {}",
        r2
    );
    assert!(
        r3.contains("Mock coordinator response"),
        "Third response missing mock text: {}",
        r3
    );
}

/// After multiple messages, chat history should contain all user messages
/// and coordinator responses.
#[test]
fn coordinator_agent_chat_history() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let mock = MockClaude::new();
    let guard = CoordinatorDaemonGuard::start(&wg_dir, &mock);

    // Send two messages
    guard.chat_ok("history test alpha", 15);
    guard.chat_ok("history test beta", 15);

    // Check JSON history
    let json_output = wg_ok_env(&wg_dir, &["chat", "--history", "--json"], &guard.env_refs());
    let parsed: serde_json::Value =
        serde_json::from_str(&json_output).expect("History JSON should be valid");
    assert!(parsed.is_array(), "JSON history should be an array");
    let arr = parsed.as_array().unwrap();

    // 2 user messages + 2 coordinator responses = 4 total
    assert_eq!(
        arr.len(),
        4,
        "Expected 4 messages in history, got {}.\nHistory: {}",
        arr.len(),
        json_output,
    );

    // Verify user messages are present
    let has_alpha = arr.iter().any(|m| {
        m["content"]
            .as_str()
            .unwrap_or("")
            .contains("history test alpha")
    });
    let has_beta = arr.iter().any(|m| {
        m["content"]
            .as_str()
            .unwrap_or("")
            .contains("history test beta")
    });
    assert!(has_alpha, "History missing 'history test alpha'");
    assert!(has_beta, "History missing 'history test beta'");

    // Verify coordinator responses are present
    let coordinator_msgs: Vec<_> = arr
        .iter()
        .filter(|m| m["role"].as_str() == Some("coordinator"))
        .collect();
    assert_eq!(
        coordinator_msgs.len(),
        2,
        "Expected 2 coordinator responses, got {}",
        coordinator_msgs.len()
    );
}

/// Coordinator cursor should advance after each processed message, preventing
/// re-processing of old messages.
#[test]
fn coordinator_agent_cursor_tracking() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let mock = MockClaude::new();
    let guard = CoordinatorDaemonGuard::start(&wg_dir, &mock);

    // Send first message
    guard.chat_ok("cursor test one", 15);
    let cursor1 = workgraph::chat::read_coordinator_cursor(&wg_dir).unwrap();
    assert!(
        cursor1 >= 1,
        "Coordinator cursor should be >= 1 after first message, got {}",
        cursor1
    );

    // Send second message
    guard.chat_ok("cursor test two", 15);
    let cursor2 = workgraph::chat::read_coordinator_cursor(&wg_dir).unwrap();
    assert!(
        cursor2 > cursor1,
        "Coordinator cursor should advance: {} -> {}",
        cursor1,
        cursor2
    );
}

/// When the coordinator agent process crashes, the daemon should:
/// 1. Write an error/timeout response for the pending request
/// 2. Detect the dead process (via try_wait or stdin write failure)
/// 3. Restart the agent process automatically
/// 4. Successfully handle subsequent messages
///
/// Uses a file-based one-shot crash trigger to avoid infinite crash loops.
/// The crash recovery context may replay recent chat messages to the new
/// process, so a message-content trigger would cause the new process to
/// crash again. The file trigger is deleted on first crash, so the restarted
/// process works normally.
#[test]
fn coordinator_agent_crash_recovery() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let mock = MockClaude::new_with_crash_trigger();

    // File-based crash trigger: when this file exists, the mock crashes and deletes it.
    let crash_file = wg_dir.join("service").join("mock_crash_trigger");
    let crash_file_str = crash_file.to_string_lossy().to_string();

    let guard = CoordinatorDaemonGuard::start_with_env(
        &wg_dir,
        &mock,
        &[("MOCK_CRASH_FILE", &crash_file_str)],
    );

    // Step 1: Normal message works (crash file doesn't exist yet)
    let r1 = guard.chat_ok("normal message before crash", 15);
    assert!(
        r1.contains("Mock coordinator response"),
        "Pre-crash message should work: {}",
        r1
    );

    // Step 2: Create the crash trigger file, then send a message.
    // The mock will see the file, delete it, and exit with code 1.
    fs::write(&crash_file, "crash").unwrap();

    let crash_output = guard.chat("this message triggers crash", 15);
    let crash_stdout = String::from_utf8_lossy(&crash_output.stdout).to_string();
    // The response should be an error/timeout message (not a mock response)
    assert!(
        crash_stdout.contains("crashed")
            || crash_stdout.contains("timed out")
            || crash_stdout.contains("error")
            || crash_stdout.contains("no response")
            || crash_stdout.contains("timeout")
            || crash_stdout.contains("Error"),
        "Crash message should produce an error response, got: {}",
        crash_stdout
    );

    // Step 3: The agent may need one more message to detect the broken pipe
    // and initiate restart (due to reap_zombies race with try_wait).
    // Send a recovery-trigger message. This will either:
    // - Get a mock response (if agent already restarted)
    // - Get an error (stdin write fails → triggers restart)
    let trigger_output = guard.chat("recovery trigger", 30);
    let _trigger_stdout = String::from_utf8_lossy(&trigger_output.stdout).to_string();

    // Step 4: Wait for the daemon to restart the agent.
    // Look for a second "Claude CLI started" in the daemon log.
    let log_path = wg_dir.join("service").join("daemon.log");
    let start = Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(25) {
            let log = read_daemon_log(&wg_dir);
            panic!(
                "Coordinator agent did not restart within 25s.\nDaemon log:\n{}",
                log
            );
        }
        if let Ok(content) = fs::read_to_string(&log_path) {
            let starts: Vec<_> = content.match_indices("Claude CLI started").collect();
            if starts.len() >= 2 {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Wait for the restarted agent to be fully ready
    std::thread::sleep(Duration::from_millis(1000));

    // Step 5: Send a normal message after recovery — should work.
    // The crash file was deleted by the mock on first crash, so the new
    // process won't crash.
    let r3 = guard.chat_ok("message after recovery", 15);
    assert!(
        r3.contains("Mock coordinator response"),
        "Post-recovery message should get a response: {}\nDaemon log:\n{}",
        r3,
        read_daemon_log(&wg_dir),
    );
}

/// When `coordinator_agent = true` but the Claude CLI is not available,
/// the daemon should fall back gracefully to Phase 1 stub responses.
#[test]
fn coordinator_agent_fallback_when_unavailable() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    enable_coordinator_agent(&wg_dir);

    // Start daemon WITHOUT mock claude on PATH — claude binary won't be found.
    // Use an empty dir as PATH prefix that doesn't contain `claude`.
    let empty_dir = TempDir::new().unwrap();
    // Remove any directory that might contain the real `claude` by using
    // only the dirs needed for basic system commands.
    let minimal_path = format!("{}:/usr/bin:/bin", empty_dir.path().display());

    // We need the wg binary's directory on PATH too (for service start to work)
    let wg_dir_path = wg_binary().parent().unwrap().to_string_lossy().to_string();
    let path_env = format!("{}:{}", wg_dir_path, minimal_path);

    let output = wg_cmd_env(
        &wg_dir,
        &["service", "start", "--interval", "600", "--max-agents", "0"],
        &[("PATH", &path_env)],
    );
    assert!(
        output.status.success(),
        "Daemon should start even without claude.\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    wait_for_socket(&wg_dir);
    // Give the daemon a moment to log the fallback
    std::thread::sleep(Duration::from_millis(500));

    // Verify the daemon log shows the fallback
    let log = read_daemon_log(&wg_dir);
    assert!(
        log.contains("Failed to spawn coordinator agent")
            || log.contains("Claude CLI not found")
            || log.contains("stub"),
        "Daemon log should mention coordinator agent failure.\nLog:\n{}",
        log
    );

    // Chat should still work via Phase 1 stub
    let output = wg_cmd_env(
        &wg_dir,
        &["chat", "fallback test message", "--timeout", "10"],
        &[("PATH", &path_env)],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "Chat should work via stub fallback.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr),
    );

    // Phase 1 stub should produce "Message received" response
    assert!(
        stdout.contains("Message received") || stdout.contains("fallback test message"),
        "Expected Phase 1 stub response, got: {}",
        stdout
    );

    stop_daemon_env(&wg_dir, &[("PATH", &path_env)]);
}

/// Verify that the response arrives quickly when the coordinator agent is active
/// (urgent wake mechanism works for Phase 2 just like Phase 1).
#[test]
fn coordinator_agent_instant_wakeup() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let mock = MockClaude::new();
    let guard = CoordinatorDaemonGuard::start(&wg_dir, &mock);

    let start = Instant::now();
    let stdout = guard.chat_ok("speed test", 10);
    let elapsed = start.elapsed();

    assert!(
        stdout.contains("Mock coordinator response"),
        "Expected mock response, got: {}",
        stdout
    );

    // Response should be fast — well under the 600s poll interval.
    // Allow generous 5s for CI environments.
    assert!(
        elapsed < Duration::from_secs(5),
        "Response took {:?}, expected < 5s (urgent wake should bypass poll interval)",
        elapsed
    );
}

/// Storage verification: inbox gets user messages, outbox gets coordinator
/// responses, request_ids match.
#[test]
fn coordinator_agent_storage_consistency() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let mock = MockClaude::new();
    let guard = CoordinatorDaemonGuard::start(&wg_dir, &mock);

    guard.chat_ok("storage consistency test", 15);

    // Verify inbox
    let inbox = workgraph::chat::read_inbox(&wg_dir).unwrap();
    assert_eq!(inbox.len(), 1, "Expected 1 inbox message");
    assert_eq!(inbox[0].role, "user");
    assert_eq!(inbox[0].content, "storage consistency test");

    // Verify outbox
    let outbox = workgraph::chat::read_outbox_since(&wg_dir, 0).unwrap();
    assert_eq!(outbox.len(), 1, "Expected 1 outbox message");
    assert_eq!(outbox[0].role, "coordinator");

    // Request IDs should correlate
    assert_eq!(
        outbox[0].request_id, inbox[0].request_id,
        "Outbox request_id should match inbox request_id"
    );

    // Outbox content should be from the mock
    assert!(
        outbox[0].content.contains("Mock coordinator response"),
        "Outbox content should be from mock, got: {}",
        outbox[0].content
    );
}

// ===========================================================================
// Real E2E tests (require Claude CLI installed, run with --ignored)
// ===========================================================================

/// Start a daemon with the real Claude CLI as coordinator agent.
fn start_real_coordinator_daemon(wg_dir: &Path) {
    enable_coordinator_agent(wg_dir);
    let output = wg_cmd(
        wg_dir,
        &["service", "start", "--interval", "600", "--max-agents", "0"],
    );
    assert!(
        output.status.success(),
        "service start failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    wait_for_socket(wg_dir);
    wait_for_coordinator_agent(wg_dir);
    // Give the real Claude CLI time to initialize
    std::thread::sleep(Duration::from_secs(2));
}

/// Real E2E: ask the coordinator to list tasks, verify it produces a
/// meaningful response about the graph state.
#[test]
#[ignore]
fn coordinator_agent_real_list_tasks() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);

    // Create a task so the graph isn't empty
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Fix the login bug",
            "-d",
            "The login form crashes on submit",
        ],
    );

    start_real_coordinator_daemon(&wg_dir);

    let output = wg_cmd(
        &wg_dir,
        &["chat", "list all open tasks", "--timeout", "120"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "chat failed.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    // The real coordinator should mention something about tasks
    let lower = stdout.to_lowercase();
    assert!(
        lower.contains("task") || lower.contains("login") || lower.contains("fix"),
        "Expected response about tasks, got: {}",
        stdout
    );

    stop_daemon(&wg_dir);
}

/// Real E2E: ask the coordinator to create a task, verify it appears in the graph.
#[test]
#[ignore]
fn coordinator_agent_real_create_task() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    start_real_coordinator_daemon(&wg_dir);

    let output = wg_cmd(
        &wg_dir,
        &[
            "chat",
            "create a task for fixing the login bug",
            "--timeout",
            "120",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "chat failed.\nstdout: {}\nstderr: {}",
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    // Give the coordinator time to execute the wg add command
    std::thread::sleep(Duration::from_secs(2));

    // Verify a task was created in the graph
    let list_output = wg_ok(&wg_dir, &["list"]);
    let lower = list_output.to_lowercase();
    assert!(
        lower.contains("login") || lower.contains("fix") || lower.contains("bug"),
        "Expected a task about login bug in the graph.\nList output: {}\nChat response: {}",
        list_output,
        stdout
    );

    stop_daemon(&wg_dir);
}

/// Real E2E: multi-turn conversation with context retention.
#[test]
#[ignore]
fn coordinator_agent_real_multi_turn() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    start_real_coordinator_daemon(&wg_dir);

    // First turn: establish context
    let r1 = wg_cmd(
        &wg_dir,
        &[
            "chat",
            "I'm working on an auth system. Can you create a task for researching JWT vs session-based auth?",
            "--timeout",
            "120",
        ],
    );
    assert!(
        r1.status.success(),
        "First message failed: {}",
        String::from_utf8_lossy(&r1.stderr)
    );

    // Second turn: reference previous context
    let r2 = wg_cmd(
        &wg_dir,
        &[
            "chat",
            "what tasks have we created so far?",
            "--timeout",
            "120",
        ],
    );
    let r2_stdout = String::from_utf8_lossy(&r2.stdout).to_string();
    assert!(
        r2.status.success(),
        "Second message failed: {}",
        String::from_utf8_lossy(&r2.stderr)
    );

    // The response should reference the auth/JWT context from the first turn
    let lower = r2_stdout.to_lowercase();
    assert!(
        lower.contains("auth")
            || lower.contains("jwt")
            || lower.contains("session")
            || lower.contains("task"),
        "Expected second response to reference auth context.\nResponse: {}",
        r2_stdout
    );

    stop_daemon(&wg_dir);
}

/// Real E2E: verify the coordinator can execute wg show and wg edit.
#[test]
#[ignore]
fn coordinator_agent_real_inspect_and_edit() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);

    // Create a task with a known ID
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Test task for inspection",
            "--id",
            "test-inspect-task",
        ],
    );

    start_real_coordinator_daemon(&wg_dir);

    // Ask to show the task
    let r1 = wg_cmd(
        &wg_dir,
        &[
            "chat",
            "show me the details of task test-inspect-task",
            "--timeout",
            "120",
        ],
    );
    let r1_stdout = String::from_utf8_lossy(&r1.stdout).to_string();
    assert!(
        r1.status.success(),
        "Show task failed: {}",
        String::from_utf8_lossy(&r1.stderr)
    );

    // Response should mention the task
    assert!(
        r1_stdout.contains("test-inspect-task") || r1_stdout.to_lowercase().contains("inspection"),
        "Expected task details in response: {}",
        r1_stdout
    );

    // Ask to edit the task
    let r2 = wg_cmd(
        &wg_dir,
        &[
            "chat",
            "add the tag 'urgent' to task test-inspect-task",
            "--timeout",
            "120",
        ],
    );
    assert!(
        r2.status.success(),
        "Edit task failed: {}",
        String::from_utf8_lossy(&r2.stderr)
    );

    // Verify the edit took effect
    std::thread::sleep(Duration::from_secs(2));
    let show = wg_ok(&wg_dir, &["show", "test-inspect-task"]);
    assert!(
        show.contains("urgent"),
        "Task should have 'urgent' tag after edit.\nShow output: {}",
        show
    );

    stop_daemon(&wg_dir);
}
