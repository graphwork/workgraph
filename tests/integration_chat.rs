//! Integration tests for Phase 1 chat foundation.
//!
//! Tests the end-to-end flow: `wg chat` → IPC → inbox → coordinator tick → outbox → response.
//! Uses a real daemon process (started via `wg service start`) in isolated temp directories.

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

/// Initialise a fresh workgraph in a temp directory and return the .workgraph path.
fn init_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);
    wg_dir
}

/// Start the daemon and wait for it to be ready (socket exists).
/// Uses a very short poll interval so tests complete quickly.
/// Disables the coordinator agent so tests use Phase 1 stub responses.
/// Returns the wg_dir for convenience.
fn start_daemon(wg_dir: &Path) -> &Path {
    let output = wg_cmd(
        wg_dir,
        &[
            "service",
            "start",
            "--interval",
            "600",
            "--max-agents",
            "0",
            "--no-coordinator-agent",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "service start failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Wait for the daemon socket to appear
    let socket = wg_dir.join("service").join("daemon.sock");
    let start = Instant::now();
    while !socket.exists() {
        if start.elapsed() > Duration::from_secs(5) {
            panic!("Daemon socket did not appear within 5s at {:?}", socket);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    wg_dir
}

/// Stop the daemon, ignoring errors (best-effort cleanup).
fn stop_daemon(wg_dir: &Path) {
    let _ = wg_cmd(wg_dir, &["service", "stop"]);
}

/// Guard that stops the daemon when dropped, ensuring cleanup.
struct DaemonGuard<'a> {
    wg_dir: &'a Path,
}

impl<'a> DaemonGuard<'a> {
    fn new(wg_dir: &'a Path) -> Self {
        start_daemon(wg_dir);
        DaemonGuard { wg_dir }
    }
}

impl Drop for DaemonGuard<'_> {
    fn drop(&mut self) {
        stop_daemon(self.wg_dir);
    }
}

// ---------------------------------------------------------------------------
// 1. Round-trip test
// ---------------------------------------------------------------------------

/// Full round-trip: wg chat sends message → IPC delivers → inbox → coordinator tick → outbox → CLI displays.
///
/// The Phase 1 coordinator stub writes a canned acknowledgement response.
#[test]
fn chat_round_trip() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    // Send a chat message with a short timeout
    let output = wg_cmd(
        &wg_dir,
        &["chat", "Hello coordinator, how are you?", "--timeout", "10"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "chat command failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Phase 1 stub echoes back with "Message received" prefix
    assert!(
        stdout.contains("Message received"),
        "Expected Phase 1 acknowledgement in stdout, got: {}",
        stdout
    );

    // Verify the original message was echoed in the response
    assert!(
        stdout.contains("Hello coordinator, how are you?"),
        "Expected original message in response, got: {}",
        stdout
    );
}

/// Verify that inbox and outbox both contain the message after a round-trip.
#[test]
fn chat_round_trip_storage() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    // Send a chat message
    let output = wg_cmd(
        &wg_dir,
        &["chat", "storage test message", "--timeout", "10"],
    );
    assert!(
        output.status.success(),
        "chat failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Check inbox has the user message
    let inbox = workgraph::chat::read_inbox(&wg_dir).unwrap();
    assert_eq!(
        inbox.len(),
        1,
        "Expected 1 inbox message, got {}",
        inbox.len()
    );
    assert_eq!(inbox[0].role, "user");
    assert_eq!(inbox[0].content, "storage test message");

    // Check outbox has the coordinator response
    let outbox = workgraph::chat::read_outbox_since(&wg_dir, 0).unwrap();
    assert_eq!(
        outbox.len(),
        1,
        "Expected 1 outbox message, got {}",
        outbox.len()
    );
    assert_eq!(outbox[0].role, "coordinator");
    assert_eq!(
        outbox[0].request_id, inbox[0].request_id,
        "Response request_id should match the original message's request_id"
    );
}

/// Verify chat history interleaves inbox and outbox messages.
#[test]
fn chat_history_after_round_trip() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    // Send two messages
    wg_cmd(&wg_dir, &["chat", "first message", "--timeout", "10"]);
    wg_cmd(&wg_dir, &["chat", "second message", "--timeout", "10"]);

    // Check history
    let output = wg_ok(&wg_dir, &["chat", "--history"]);
    assert!(
        output.contains("first message"),
        "History should contain first message, got: {}",
        output
    );
    assert!(
        output.contains("second message"),
        "History should contain second message, got: {}",
        output
    );

    // JSON history should be valid
    let json_output = wg_ok(&wg_dir, &["chat", "--history", "--json"]);
    let parsed: serde_json::Value =
        serde_json::from_str(&json_output).expect("History JSON should be valid");
    assert!(parsed.is_array(), "JSON history should be an array");
    let arr = parsed.as_array().unwrap();
    // 2 user messages + 2 coordinator responses = 4 total
    assert_eq!(
        arr.len(),
        4,
        "Expected 4 messages in history, got {}",
        arr.len()
    );
}

// ---------------------------------------------------------------------------
// 2. Instant wake-up test
// ---------------------------------------------------------------------------

/// Verify the coordinator tick fires quickly after UserChat (instant wake-up),
/// not waiting for the full poll interval.
///
/// The daemon is started with poll_interval=600s (10 min). If the urgent wake
/// mechanism works, the response should arrive in well under 5s. If it's broken,
/// the test would hang for 600s (and we timeout at 5s).
#[test]
fn chat_instant_wakeup() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    let start = Instant::now();

    // Send a message via the binary. The daemon poll_interval is 600s,
    // so if the response comes back quickly it must be due to urgent_wake.
    let output = wg_cmd(&wg_dir, &["chat", "instant wake test", "--timeout", "5"]);
    let elapsed = start.elapsed();

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        output.status.success(),
        "chat should succeed, got stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Message received"),
        "Should get Phase 1 response, got: {}",
        stdout
    );

    // The response should arrive quickly — well under the poll_interval.
    // Allow generous 3s for CI environments (actual should be < 500ms).
    assert!(
        elapsed < Duration::from_secs(3),
        "Response took {:?}, expected < 3s (urgent wake should bypass 600s poll interval)",
        elapsed
    );
}

// ---------------------------------------------------------------------------
// 3. Concurrent chat test
// ---------------------------------------------------------------------------

/// Multiple chat messages sent in parallel should each get the correct response
/// correlated by request_id.
#[test]
fn chat_concurrent_messages() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    let num_messages = 5;
    let mut handles = Vec::new();

    for i in 0..num_messages {
        let wg_dir = wg_dir.clone();
        let binary = wg_binary();
        handles.push(std::thread::spawn(move || {
            let msg = format!("concurrent message {}", i);
            let output = Command::new(&binary)
                .arg("--dir")
                .arg(&wg_dir)
                .args(["chat", &msg, "--timeout", "15"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .unwrap();

            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            (i, output.status.success(), stdout, stderr)
        }));
    }

    let mut results: Vec<(usize, bool, String, String)> = Vec::new();
    for h in handles {
        results.push(h.join().unwrap());
    }

    // All should succeed
    for (i, success, stdout, stderr) in &results {
        assert!(
            *success,
            "Message {} failed.\nstdout: {}\nstderr: {}",
            i, stdout, stderr
        );
    }

    // Each response should contain its original message (proving request_id correlation)
    for (i, _, stdout, _) in &results {
        let expected = format!("concurrent message {}", i);
        assert!(
            stdout.contains(&expected),
            "Response for message {} should contain the original text.\nExpected to find: {}\nGot: {}",
            i,
            expected,
            stdout
        );
    }

    // Verify storage has all messages
    let inbox = workgraph::chat::read_inbox(&wg_dir).unwrap();
    assert_eq!(
        inbox.len(),
        num_messages,
        "Inbox should have {} messages, got {}",
        num_messages,
        inbox.len()
    );

    let outbox = workgraph::chat::read_outbox_since(&wg_dir, 0).unwrap();
    assert_eq!(
        outbox.len(),
        num_messages,
        "Outbox should have {} messages, got {}",
        num_messages,
        outbox.len()
    );

    // Each inbox message should have a matching outbox response with the same request_id
    for inbox_msg in &inbox {
        let matching = outbox.iter().find(|o| o.request_id == inbox_msg.request_id);
        assert!(
            matching.is_some(),
            "No outbox response found for request_id '{}'",
            inbox_msg.request_id
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Error path tests
// ---------------------------------------------------------------------------

/// `wg chat` should fail gracefully when the service is not running.
#[test]
fn chat_error_service_not_running() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    // Do NOT start the daemon

    let output = wg_cmd(&wg_dir, &["chat", "test message", "--timeout", "2"]);
    assert!(
        !output.status.success(),
        "chat should fail when service is not running"
    );

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    // Should mention service/connection failure
    assert!(
        stderr.contains("service")
            || stderr.contains("connect")
            || stderr.contains("running")
            || stderr.contains("Start"),
        "Error should mention service connectivity, got stderr: {}",
        stderr
    );
}

/// `wg chat` with an empty message should be rejected.
#[test]
fn chat_error_empty_message() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    // No daemon needed — validation happens before IPC

    let output = wg_cmd(&wg_dir, &["chat", "  ", "--timeout", "2"]);
    assert!(
        !output.status.success(),
        "chat should fail with empty message"
    );

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("empty") || stderr.contains("Empty"),
        "Error should mention empty message, got: {}",
        stderr
    );
}

/// Verify that `wg chat --clear` clears all chat data.
#[test]
fn chat_clear_works() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    // Send a message first
    let output = wg_cmd(
        &wg_dir,
        &["chat", "message before clear", "--timeout", "10"],
    );
    assert!(output.status.success());

    // Verify data exists
    let inbox = workgraph::chat::read_inbox(&wg_dir).unwrap();
    assert!(!inbox.is_empty(), "Inbox should have messages before clear");

    // Clear
    let output = wg_ok(&wg_dir, &["chat", "--clear"]);
    assert!(
        output.contains("cleared") || output.contains("Cleared"),
        "Clear output should confirm, got: {}",
        output
    );

    // Verify data is gone
    let inbox = workgraph::chat::read_inbox(&wg_dir).unwrap();
    assert!(inbox.is_empty(), "Inbox should be empty after clear");
    let outbox = workgraph::chat::read_outbox_since(&wg_dir, 0).unwrap();
    assert!(outbox.is_empty(), "Outbox should be empty after clear");
}

// ---------------------------------------------------------------------------
// 5. Coordinator cursor tracking
// ---------------------------------------------------------------------------

/// After the coordinator processes messages, its cursor should advance so
/// it doesn't re-process old messages on the next tick.
#[test]
fn chat_coordinator_cursor_advances() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    // Send first message
    let output = wg_cmd(&wg_dir, &["chat", "cursor test 1", "--timeout", "10"]);
    assert!(output.status.success());

    // Check coordinator cursor advanced
    let cursor = workgraph::chat::read_coordinator_cursor(&wg_dir).unwrap();
    assert!(
        cursor >= 1,
        "Coordinator cursor should have advanced to >= 1, got {}",
        cursor
    );

    // Send second message
    let output = wg_cmd(&wg_dir, &["chat", "cursor test 2", "--timeout", "10"]);
    assert!(output.status.success());

    let cursor2 = workgraph::chat::read_coordinator_cursor(&wg_dir).unwrap();
    assert!(
        cursor2 > cursor,
        "Coordinator cursor should have advanced further: {} -> {}",
        cursor,
        cursor2
    );
}
