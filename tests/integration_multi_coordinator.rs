//! Integration tests for multi-coordinator support.
//!
//! Verifies that:
//! - `wg chat --coordinator N` routes messages to the correct coordinator
//! - Messages don't leak between coordinators
//! - Both coordinators produce isolated responses
//! - Chat history is per-coordinator

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers (mirrors integration_chat.rs)
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

fn init_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);
    wg_dir
}

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

fn stop_daemon(wg_dir: &Path) {
    let _ = wg_cmd(wg_dir, &["service", "stop"]);
}

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
// 1. Coordinator 1 round-trip: --coordinator 1 routes correctly
// ---------------------------------------------------------------------------

/// `wg chat --coordinator 1` sends to coordinator 1 and gets a response.
#[test]
fn multi_coordinator_chat_coordinator_1_round_trip() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    let output = wg_cmd(
        &wg_dir,
        &[
            "chat",
            "hello coordinator one",
            "--coordinator",
            "1",
            "--timeout",
            "10",
        ],
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(
        output.status.success(),
        "chat --coordinator 1 failed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Stub response should echo the message
    assert!(
        stdout.contains("hello coordinator one"),
        "Response should contain the original message, got: {}",
        stdout
    );
    assert!(
        stdout.contains("Message received"),
        "Should get stub acknowledgement, got: {}",
        stdout
    );
}

// ---------------------------------------------------------------------------
// 2. Isolation: messages don't leak between coordinators
// ---------------------------------------------------------------------------

/// Messages sent to coordinator 0 should NOT appear in coordinator 1's inbox,
/// and vice versa.
#[test]
fn multi_coordinator_message_isolation() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    // Send a message to coordinator 0
    let output = wg_cmd(
        &wg_dir,
        &[
            "chat",
            "message for coordinator zero",
            "--coordinator",
            "0",
            "--timeout",
            "10",
        ],
    );
    assert!(
        output.status.success(),
        "chat to coordinator 0 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Send a message to coordinator 1
    let output = wg_cmd(
        &wg_dir,
        &[
            "chat",
            "message for coordinator one",
            "--coordinator",
            "1",
            "--timeout",
            "10",
        ],
    );
    assert!(
        output.status.success(),
        "chat to coordinator 1 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify coordinator 0's inbox only has its message
    let inbox_0 = workgraph::chat::read_inbox_for(&wg_dir, 0).unwrap();
    assert_eq!(
        inbox_0.len(),
        1,
        "Coordinator 0 should have 1 inbox message"
    );
    assert_eq!(inbox_0[0].content, "message for coordinator zero");

    // Verify coordinator 1's inbox only has its message
    let inbox_1 = workgraph::chat::read_inbox_for(&wg_dir, 1).unwrap();
    assert_eq!(
        inbox_1.len(),
        1,
        "Coordinator 1 should have 1 inbox message"
    );
    assert_eq!(inbox_1[0].content, "message for coordinator one");

    // Verify outboxes are also isolated
    let outbox_0 = workgraph::chat::read_outbox_since_for(&wg_dir, 0, 0).unwrap();
    assert_eq!(
        outbox_0.len(),
        1,
        "Coordinator 0 should have 1 outbox message"
    );
    assert!(
        outbox_0[0].content.contains("coordinator zero"),
        "Coordinator 0 outbox should reference its message, got: {}",
        outbox_0[0].content
    );

    let outbox_1 = workgraph::chat::read_outbox_since_for(&wg_dir, 1, 0).unwrap();
    assert_eq!(
        outbox_1.len(),
        1,
        "Coordinator 1 should have 1 outbox message"
    );
    assert!(
        outbox_1[0].content.contains("coordinator one"),
        "Coordinator 1 outbox should reference its message, got: {}",
        outbox_1[0].content
    );

    // Cross-check: coordinator 0's outbox should NOT contain coordinator 1's message
    assert!(
        !outbox_0[0].content.contains("coordinator one"),
        "Coordinator 0 outbox should not contain coordinator 1's message"
    );
    assert!(
        !outbox_1[0].content.contains("coordinator zero"),
        "Coordinator 1 outbox should not contain coordinator 0's message"
    );
}

// ---------------------------------------------------------------------------
// 3. History isolation
// ---------------------------------------------------------------------------

/// `wg chat --history --coordinator N` shows only that coordinator's history.
#[test]
fn multi_coordinator_history_isolation() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    // Send messages to different coordinators
    wg_cmd(
        &wg_dir,
        &[
            "chat",
            "alpha message",
            "--coordinator",
            "0",
            "--timeout",
            "10",
        ],
    );
    wg_cmd(
        &wg_dir,
        &[
            "chat",
            "beta message",
            "--coordinator",
            "1",
            "--timeout",
            "10",
        ],
    );

    // Check coordinator 0 history
    let history_0 = wg_ok(&wg_dir, &["chat", "--history", "--coordinator", "0"]);
    assert!(
        history_0.contains("alpha message"),
        "Coordinator 0 history should contain alpha, got: {}",
        history_0
    );
    assert!(
        !history_0.contains("beta message"),
        "Coordinator 0 history should NOT contain beta, got: {}",
        history_0
    );

    // Check coordinator 1 history
    let history_1 = wg_ok(&wg_dir, &["chat", "--history", "--coordinator", "1"]);
    assert!(
        history_1.contains("beta message"),
        "Coordinator 1 history should contain beta, got: {}",
        history_1
    );
    assert!(
        !history_1.contains("alpha message"),
        "Coordinator 1 history should NOT contain alpha, got: {}",
        history_1
    );
}

// ---------------------------------------------------------------------------
// 4. Clear isolation
// ---------------------------------------------------------------------------

/// Clearing one coordinator's history does not affect another's.
#[test]
fn multi_coordinator_clear_isolation() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    // Send messages to both coordinators
    wg_cmd(
        &wg_dir,
        &[
            "chat",
            "coord0 msg",
            "--coordinator",
            "0",
            "--timeout",
            "10",
        ],
    );
    wg_cmd(
        &wg_dir,
        &[
            "chat",
            "coord1 msg",
            "--coordinator",
            "1",
            "--timeout",
            "10",
        ],
    );

    // Clear coordinator 0
    wg_ok(&wg_dir, &["chat", "--clear", "--coordinator", "0"]);

    // Coordinator 0 should be empty
    let inbox_0 = workgraph::chat::read_inbox_for(&wg_dir, 0).unwrap();
    assert!(inbox_0.is_empty(), "Coordinator 0 inbox should be cleared");

    // Coordinator 1 should still have its message
    let inbox_1 = workgraph::chat::read_inbox_for(&wg_dir, 1).unwrap();
    assert_eq!(
        inbox_1.len(),
        1,
        "Coordinator 1 inbox should be unaffected by coordinator 0 clear"
    );
    assert_eq!(inbox_1[0].content, "coord1 msg");
}

// ---------------------------------------------------------------------------
// 5. Both coordinators in storage
// ---------------------------------------------------------------------------

/// Both coordinator directories exist after sending messages to each.
#[test]
fn multi_coordinator_both_visible_in_storage() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    // Send to coordinator 0
    wg_cmd(
        &wg_dir,
        &["chat", "msg0", "--coordinator", "0", "--timeout", "10"],
    );
    // Send to coordinator 1
    wg_cmd(
        &wg_dir,
        &["chat", "msg1", "--coordinator", "1", "--timeout", "10"],
    );

    // Both coordinator chat directories should exist
    let chat_dir_0 = wg_dir.join("chat").join("0");
    let chat_dir_1 = wg_dir.join("chat").join("1");
    assert!(
        chat_dir_0.exists(),
        "Coordinator 0 chat directory should exist"
    );
    assert!(
        chat_dir_1.exists(),
        "Coordinator 1 chat directory should exist"
    );

    // Both should have inbox and outbox files
    assert!(chat_dir_0.join("inbox.jsonl").exists());
    assert!(chat_dir_0.join("outbox.jsonl").exists());
    assert!(chat_dir_1.join("inbox.jsonl").exists());
    assert!(chat_dir_1.join("outbox.jsonl").exists());
}

// ---------------------------------------------------------------------------
// 6. Multiple messages to same coordinator
// ---------------------------------------------------------------------------

/// Multiple messages to the same coordinator accumulate correctly.
#[test]
fn multi_coordinator_multiple_messages_same_coordinator() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    // Send 3 messages to coordinator 1
    for i in 0..3 {
        let msg = format!("coordinator-1 message {}", i);
        let output = wg_cmd(
            &wg_dir,
            &["chat", &msg, "--coordinator", "1", "--timeout", "10"],
        );
        assert!(
            output.status.success(),
            "Message {} to coordinator 1 failed: {}",
            i,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Coordinator 1 should have 3 inbox messages
    let inbox_1 = workgraph::chat::read_inbox_for(&wg_dir, 1).unwrap();
    assert_eq!(
        inbox_1.len(),
        3,
        "Coordinator 1 should have 3 inbox messages, got {}",
        inbox_1.len()
    );

    // Coordinator 0 should have no messages
    let inbox_0 = workgraph::chat::read_inbox_for(&wg_dir, 0).unwrap();
    assert!(
        inbox_0.is_empty(),
        "Coordinator 0 should have no messages, got {}",
        inbox_0.len()
    );

    // Coordinator 1 should have 3 outbox responses
    let outbox_1 = workgraph::chat::read_outbox_since_for(&wg_dir, 1, 0).unwrap();
    assert_eq!(
        outbox_1.len(),
        3,
        "Coordinator 1 should have 3 outbox messages, got {}",
        outbox_1.len()
    );
}

// ---------------------------------------------------------------------------
// 7. Default coordinator is 0
// ---------------------------------------------------------------------------

/// When --coordinator is not specified, messages go to coordinator 0.
#[test]
fn multi_coordinator_default_is_zero() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = init_workgraph(&tmp);
    let _guard = DaemonGuard::new(&wg_dir);

    // Send without --coordinator flag
    let output = wg_cmd(
        &wg_dir,
        &["chat", "default coordinator message", "--timeout", "10"],
    );
    assert!(output.status.success());

    // Should go to coordinator 0
    let inbox_0 = workgraph::chat::read_inbox_for(&wg_dir, 0).unwrap();
    assert_eq!(inbox_0.len(), 1);
    assert_eq!(inbox_0[0].content, "default coordinator message");

    // Coordinator 1 should be empty
    let inbox_1 = workgraph::chat::read_inbox_for(&wg_dir, 1).unwrap();
    assert!(inbox_1.is_empty());
}
