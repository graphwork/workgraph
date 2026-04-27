#![allow(clippy::field_reassign_with_default)]
//! End-to-end integration tests for the agent message queue system.
//!
//! Tests exercise real `wg` CLI commands in isolated temp directories.
//! Validates: message storage, CLI commands, pending task pickup,
//! running agent message delivery, edge cases, coordinator integration,
//! and the full end-to-end smoke flow.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::Status;
use workgraph::messages;
use workgraph::parser::load_graph;

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

fn wg_fail(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    assert!(
        !output.status.success(),
        "wg {:?} should have failed but succeeded.\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    format!("{}{}", stdout, stderr)
}

/// Initialize a workgraph in a temp directory and return the .workgraph path.
fn init_wg() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);
    assert!(wg_dir.exists());
    (tmp, wg_dir)
}

// ===========================================================================
// 1. MESSAGE STORAGE
// ===========================================================================

#[test]
fn msg_storage_send_and_persist() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Storage test", "--id", "store-1"]);

    // Send two messages via the Rust API directly
    let id1 =
        messages::send_message(&wg_dir, "store-1", "First message", "user", "normal").unwrap();
    let id2 = messages::send_message(
        &wg_dir,
        "store-1",
        "Second message",
        "coordinator",
        "urgent",
    )
    .unwrap();

    assert_eq!(id1, 1);
    assert_eq!(id2, 2);

    // Verify persistence: read back and check content
    let msgs = messages::list_messages(&wg_dir, "store-1").unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].id, 1);
    assert_eq!(msgs[0].body, "First message");
    assert_eq!(msgs[0].sender, "user");
    assert_eq!(msgs[0].priority, "normal");
    assert_eq!(msgs[1].id, 2);
    assert_eq!(msgs[1].body, "Second message");
    assert_eq!(msgs[1].sender, "coordinator");
    assert_eq!(msgs[1].priority, "urgent");
}

#[test]
fn msg_storage_ordering_is_by_id() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Order test", "--id", "order-1"]);

    for i in 1..=10 {
        messages::send_message(&wg_dir, "order-1", &format!("Msg {}", i), "user", "normal")
            .unwrap();
    }

    let msgs = messages::list_messages(&wg_dir, "order-1").unwrap();
    assert_eq!(msgs.len(), 10);
    for (i, msg) in msgs.iter().enumerate() {
        assert_eq!(msg.id, (i + 1) as u64, "Message IDs should be sequential");
        assert_eq!(msg.body, format!("Msg {}", i + 1));
    }
}

#[test]
fn msg_storage_read_unread_tracking() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Unread test", "--id", "unread-1"]);

    messages::send_message(&wg_dir, "unread-1", "Alpha", "user", "normal").unwrap();
    messages::send_message(&wg_dir, "unread-1", "Beta", "user", "normal").unwrap();

    // First read: both messages unread
    let unread = messages::read_unread(&wg_dir, "unread-1", "agent-x").unwrap();
    assert_eq!(unread.len(), 2, "Both messages should be unread initially");
    assert_eq!(unread[0].body, "Alpha");
    assert_eq!(unread[1].body, "Beta");

    // Second read: cursor advanced, nothing new
    let unread = messages::read_unread(&wg_dir, "unread-1", "agent-x").unwrap();
    assert!(unread.is_empty(), "No new messages after reading all");

    // Send a third message
    messages::send_message(&wg_dir, "unread-1", "Gamma", "user", "normal").unwrap();

    // Third read: only the new message
    let unread = messages::read_unread(&wg_dir, "unread-1", "agent-x").unwrap();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].body, "Gamma");
}

#[test]
fn msg_storage_separate_cursors_per_agent() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Multi-agent", "--id", "multi-1"]);

    messages::send_message(&wg_dir, "multi-1", "Shared msg", "user", "normal").unwrap();

    // agent-a reads it
    let unread_a = messages::read_unread(&wg_dir, "multi-1", "agent-a").unwrap();
    assert_eq!(unread_a.len(), 1);

    // agent-b hasn't read yet — should still see it
    let unread_b = messages::read_unread(&wg_dir, "multi-1", "agent-b").unwrap();
    assert_eq!(unread_b.len(), 1);

    // agent-a has no more unread
    let unread_a2 = messages::read_unread(&wg_dir, "multi-1", "agent-a").unwrap();
    assert!(unread_a2.is_empty());
}

#[test]
fn msg_storage_poll_does_not_advance_cursor() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Poll test", "--id", "poll-1"]);

    messages::send_message(&wg_dir, "poll-1", "Peek", "user", "normal").unwrap();

    // Poll returns messages but doesn't advance cursor
    let polled1 = messages::poll_messages(&wg_dir, "poll-1", "agent-p").unwrap();
    assert_eq!(polled1.len(), 1);

    // Poll again: still the same message
    let polled2 = messages::poll_messages(&wg_dir, "poll-1", "agent-p").unwrap();
    assert_eq!(polled2.len(), 1);

    // Now read: advances cursor
    let read = messages::read_unread(&wg_dir, "poll-1", "agent-p").unwrap();
    assert_eq!(read.len(), 1);

    // Poll after read: no new messages
    let polled3 = messages::poll_messages(&wg_dir, "poll-1", "agent-p").unwrap();
    assert!(polled3.is_empty());
}

#[test]
fn msg_storage_timestamps_are_valid_rfc3339() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Timestamp test", "--id", "ts-1"]);

    messages::send_message(&wg_dir, "ts-1", "Check timestamp", "user", "normal").unwrap();

    let msgs = messages::list_messages(&wg_dir, "ts-1").unwrap();
    assert_eq!(msgs.len(), 1);
    chrono::DateTime::parse_from_rfc3339(&msgs[0].timestamp)
        .expect("timestamp should be valid RFC 3339");
}

// ===========================================================================
// 2. CLI COMMANDS
// ===========================================================================

#[test]
fn cli_msg_send_and_list() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "CLI send test", "--id", "cli-send"]);

    // Send via CLI
    let output = wg_ok(&wg_dir, &["msg", "send", "cli-send", "Hello from CLI"]);
    assert!(
        output.contains("#1"),
        "Should report message ID #1, got: {}",
        output
    );
    assert!(
        output.contains("cli-send"),
        "Should confirm task id, got: {}",
        output
    );

    // Send with priority
    let output = wg_ok(
        &wg_dir,
        &[
            "msg",
            "send",
            "cli-send",
            "Urgent!",
            "--priority",
            "urgent",
            "--from",
            "coordinator",
        ],
    );
    assert!(
        output.contains("#2"),
        "Should report message ID #2, got: {}",
        output
    );

    // List messages
    let output = wg_ok(&wg_dir, &["msg", "list", "cli-send"]);
    assert!(
        output.contains("Hello from CLI"),
        "List should show first message, got: {}",
        output
    );
    assert!(
        output.contains("Urgent!"),
        "List should show second message, got: {}",
        output
    );
    assert!(
        output.contains("[URGENT]"),
        "List should show urgent marker, got: {}",
        output
    );
    assert!(
        output.contains("2 total"),
        "Should show total count, got: {}",
        output
    );
}

#[test]
fn cli_msg_list_json() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "JSON test", "--id", "json-1"]);
    wg_ok(&wg_dir, &["msg", "send", "json-1", "JSON body"]);

    let output = wg_ok(&wg_dir, &["msg", "list", "json-1", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap_or_else(|e| {
        panic!(
            "List --json output should be valid JSON: {}\nOutput: {}",
            e, output
        )
    });
    assert!(parsed.is_array());
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["body"], "JSON body");
}

#[test]
fn cli_msg_read_advances_cursor() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Read test", "--id", "read-1"]);
    wg_ok(&wg_dir, &["msg", "send", "read-1", "First"]);
    wg_ok(&wg_dir, &["msg", "send", "read-1", "Second"]);

    // Read with explicit agent
    let output = wg_ok(&wg_dir, &["msg", "read", "read-1", "--agent", "test-agent"]);
    assert!(
        output.contains("First"),
        "Should show first message, got: {}",
        output
    );
    assert!(
        output.contains("Second"),
        "Should show second message, got: {}",
        output
    );
    assert!(
        output.contains("2 unread"),
        "Should show unread count, got: {}",
        output
    );

    // Read again: no unread messages
    let output = wg_ok(&wg_dir, &["msg", "read", "read-1", "--agent", "test-agent"]);
    assert!(
        output.contains("No unread"),
        "Should show no unread messages, got: {}",
        output
    );

    // Send a third and read again
    wg_ok(&wg_dir, &["msg", "send", "read-1", "Third"]);
    let output = wg_ok(&wg_dir, &["msg", "read", "read-1", "--agent", "test-agent"]);
    assert!(
        output.contains("Third"),
        "Should show new message, got: {}",
        output
    );
    assert!(
        !output.contains("First"),
        "Should NOT re-show old messages, got: {}",
        output
    );
}

#[test]
fn cli_msg_poll_exit_codes() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Poll test", "--id", "poll-cli"]);

    // Poll with no messages: exit code 1
    let output = wg_cmd(
        &wg_dir,
        &["msg", "poll", "poll-cli", "--agent", "test-agent"],
    );
    assert!(
        !output.status.success(),
        "Poll should exit non-zero when no messages"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No new messages"),
        "Should say no new messages, got: {}",
        stdout
    );

    // Send a message, then poll: exit code 0
    wg_ok(&wg_dir, &["msg", "send", "poll-cli", "New!"]);
    let output = wg_cmd(
        &wg_dir,
        &["msg", "poll", "poll-cli", "--agent", "test-agent"],
    );
    assert!(
        output.status.success(),
        "Poll should exit 0 when new messages exist"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("New!"),
        "Poll should show message content, got: {}",
        stdout
    );

    // Poll again: still shows message (poll doesn't advance cursor)
    let output = wg_cmd(
        &wg_dir,
        &["msg", "poll", "poll-cli", "--agent", "test-agent"],
    );
    assert!(
        output.status.success(),
        "Poll should still show messages (doesn't advance cursor)"
    );
}

#[test]
fn cli_msg_poll_json() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Poll JSON", "--id", "poll-json"]);
    wg_ok(&wg_dir, &["msg", "send", "poll-json", "JSON poll"]);

    let output = wg_ok(
        &wg_dir,
        &["msg", "poll", "poll-json", "--agent", "ag", "--json"],
    );
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap_or_else(|e| {
        panic!(
            "Poll --json should be valid JSON: {}\nOutput: {}",
            e, output
        )
    });
    assert!(parsed.is_array());
    assert_eq!(parsed.as_array().unwrap().len(), 1);
}

#[test]
fn cli_msg_list_empty() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Empty list", "--id", "empty-1"]);

    let output = wg_ok(&wg_dir, &["msg", "list", "empty-1"]);
    assert!(
        output.contains("No messages"),
        "Should indicate no messages, got: {}",
        output
    );
}

// ===========================================================================
// 3. PENDING TASK PICKUP (queued messages in prompt context)
// ===========================================================================

#[test]
fn pending_task_queued_messages_in_context() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Pending pickup", "--id", "pending-1"]);

    // Send messages BEFORE the task is claimed
    messages::send_message(&wg_dir, "pending-1", "Pre-claim context", "user", "normal").unwrap();
    messages::send_message(
        &wg_dir,
        "pending-1",
        "Urgent pre-claim",
        "coordinator",
        "urgent",
    )
    .unwrap();

    // format_queued_messages should include them
    let formatted = messages::format_queued_messages(&wg_dir, "pending-1");
    assert!(
        formatted.contains("## Queued Messages"),
        "Should have header, got: {}",
        formatted
    );
    assert!(
        formatted.contains("Pre-claim context"),
        "Should include first message, got: {}",
        formatted
    );
    assert!(
        formatted.contains("Urgent pre-claim"),
        "Should include second message, got: {}",
        formatted
    );
    assert!(
        formatted.contains("[URGENT]"),
        "Should show urgent marker, got: {}",
        formatted
    );
}

#[test]
fn pending_task_cursor_advancement_at_spawn() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Cursor advance", "--id", "cursor-1"]);

    // Send messages before spawn
    messages::send_message(&wg_dir, "cursor-1", "Pre-spawn 1", "user", "normal").unwrap();
    messages::send_message(&wg_dir, "cursor-1", "Pre-spawn 2", "user", "normal").unwrap();

    // Simulate what execution.rs does at spawn: advance cursor
    let all_msgs = messages::list_messages(&wg_dir, "cursor-1").unwrap();
    let last_id = all_msgs.last().unwrap().id;
    messages::write_cursor(&wg_dir, "spawn-agent-1", "cursor-1", last_id).unwrap();

    // Agent now reads: should see nothing (cursor was advanced past pre-spawn messages)
    let unread = messages::read_unread(&wg_dir, "cursor-1", "spawn-agent-1").unwrap();
    assert!(
        unread.is_empty(),
        "Agent should have no unread after cursor advancement at spawn"
    );

    // New message after spawn: agent should see it
    messages::send_message(&wg_dir, "cursor-1", "Post-spawn", "user", "normal").unwrap();
    let unread = messages::read_unread(&wg_dir, "cursor-1", "spawn-agent-1").unwrap();
    assert_eq!(unread.len(), 1);
    assert_eq!(unread[0].body, "Post-spawn");
}

#[test]
fn pending_task_empty_queue_formats_as_empty_string() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "No messages", "--id", "no-msg"]);

    let formatted = messages::format_queued_messages(&wg_dir, "no-msg");
    assert!(
        formatted.is_empty(),
        "No messages should format as empty string"
    );
}

// ===========================================================================
// 4. RUNNING AGENT MESSAGE DELIVERY (adapter notification files)
// ===========================================================================

#[test]
fn adapter_claude_writes_notification_file() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Adapter test", "--id", "adapt-1"]);

    let agent = workgraph::service::registry::AgentEntry {
        id: "agent-claude-1".to_string(),
        pid: 99999,
        task_id: "adapt-1".to_string(),
        executor: "claude".to_string(),
        started_at: "2026-02-28T00:00:00Z".to_string(),
        last_heartbeat: "2026-02-28T00:00:00Z".to_string(),
        status: workgraph::service::registry::AgentStatus::Working,
        output_file: "/tmp/output.log".to_string(),
        model: None,
        completed_at: None,
        worktree_path: None,
    };

    // Deliver message via the full deliver_message path
    let (msg_id, delivered) = messages::deliver_message(
        &wg_dir,
        "adapt-1",
        &agent,
        "Mid-execution update",
        "user",
        "normal",
    )
    .unwrap();

    assert_eq!(msg_id, 1, "First message should have ID 1");
    assert!(
        !delivered,
        "Claude adapter should not support realtime delivery"
    );

    // Verify message is in persistent queue
    let msgs = messages::list_messages(&wg_dir, "adapt-1").unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].body, "Mid-execution update");

    // Verify notification file was written
    let notif_path = wg_dir
        .join("agents")
        .join("agent-claude-1")
        .join("pending_messages.txt");
    assert!(notif_path.exists(), "Notification file should exist");
    let content = fs::read_to_string(&notif_path).unwrap();
    assert!(
        content.contains("Mid-execution update"),
        "Notification should contain message body, got: {}",
        content
    );
}

#[test]
fn adapter_amplifier_writes_notification_file() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Amplifier test", "--id", "amp-1"]);

    let agent = workgraph::service::registry::AgentEntry {
        id: "agent-amp-1".to_string(),
        pid: 88888,
        task_id: "amp-1".to_string(),
        executor: "amplifier".to_string(),
        started_at: "2026-02-28T00:00:00Z".to_string(),
        last_heartbeat: "2026-02-28T00:00:00Z".to_string(),
        status: workgraph::service::registry::AgentStatus::Working,
        output_file: "/tmp/output.log".to_string(),
        model: None,
        completed_at: None,
        worktree_path: None,
    };

    let (msg_id, delivered) = messages::deliver_message(
        &wg_dir,
        "amp-1",
        &agent,
        "Amplifier context update",
        "coordinator",
        "urgent",
    )
    .unwrap();

    assert_eq!(msg_id, 1);
    assert!(
        !delivered,
        "Amplifier adapter should not support realtime delivery"
    );

    let notif_path = wg_dir
        .join("agents")
        .join("agent-amp-1")
        .join("pending_messages.txt");
    assert!(notif_path.exists());
    let content = fs::read_to_string(&notif_path).unwrap();
    assert!(content.contains("Amplifier context update"));
    assert!(content.contains("[URGENT]"));
}

#[test]
fn adapter_shell_writes_notification_file() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Shell test", "--id", "shell-1"]);

    let agent = workgraph::service::registry::AgentEntry {
        id: "agent-shell-1".to_string(),
        pid: 77777,
        task_id: "shell-1".to_string(),
        executor: "shell".to_string(),
        started_at: "2026-02-28T00:00:00Z".to_string(),
        last_heartbeat: "2026-02-28T00:00:00Z".to_string(),
        status: workgraph::service::registry::AgentStatus::Working,
        output_file: "/tmp/output.log".to_string(),
        model: None,
        completed_at: None,
        worktree_path: None,
    };

    let (msg_id, delivered) = messages::deliver_message(
        &wg_dir,
        "shell-1",
        &agent,
        "Shell message",
        "user",
        "normal",
    )
    .unwrap();

    assert_eq!(msg_id, 1);
    assert!(
        !delivered,
        "Shell adapter should not support realtime delivery"
    );

    let notif_path = wg_dir
        .join("agents")
        .join("agent-shell-1")
        .join("pending_messages.txt");
    assert!(notif_path.exists());
    let content = fs::read_to_string(&notif_path).unwrap();
    assert!(content.contains("Shell message"));
}

#[test]
fn adapter_factory_returns_correct_types() {
    let claude = messages::adapter_for_executor("claude");
    assert_eq!(claude.executor_type(), "claude");
    assert!(!claude.supports_realtime());

    let amplifier = messages::adapter_for_executor("amplifier");
    assert_eq!(amplifier.executor_type(), "amplifier");
    assert!(!amplifier.supports_realtime());

    let shell = messages::adapter_for_executor("shell");
    assert_eq!(shell.executor_type(), "shell");
    assert!(!shell.supports_realtime());

    // Unknown defaults to claude
    let unknown = messages::adapter_for_executor("mystery-executor");
    assert_eq!(unknown.executor_type(), "claude");
}

#[test]
fn adapter_notification_accumulates_multiple_messages() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Accumulate", "--id", "accum-1"]);

    let agent = workgraph::service::registry::AgentEntry {
        id: "agent-accum".to_string(),
        pid: 66666,
        task_id: "accum-1".to_string(),
        executor: "claude".to_string(),
        started_at: "2026-02-28T00:00:00Z".to_string(),
        last_heartbeat: "2026-02-28T00:00:00Z".to_string(),
        status: workgraph::service::registry::AgentStatus::Working,
        output_file: "/tmp/output.log".to_string(),
        model: None,
        completed_at: None,
        worktree_path: None,
    };

    // Deliver 5 messages
    for i in 1..=5 {
        messages::deliver_message(
            &wg_dir,
            "accum-1",
            &agent,
            &format!("Message {}", i),
            "user",
            "normal",
        )
        .unwrap();
    }

    // All 5 should be in the notification file
    let notif_path = wg_dir
        .join("agents")
        .join("agent-accum")
        .join("pending_messages.txt");
    let content = fs::read_to_string(&notif_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 5, "Should have 5 notification lines");
    for i in 1..=5 {
        assert!(
            content.contains(&format!("Message {}", i)),
            "Should contain message {}, got: {}",
            i,
            content
        );
    }

    // All 5 should also be in the persistent queue
    let msgs = messages::list_messages(&wg_dir, "accum-1").unwrap();
    assert_eq!(msgs.len(), 5);
}

// ===========================================================================
// 5. EDGE CASES
// ===========================================================================

#[test]
fn edge_message_to_nonexistent_task_errors() {
    let (_tmp, wg_dir) = init_wg();

    // CLI send to nonexistent task should fail
    let combined = wg_fail(&wg_dir, &["msg", "send", "does-not-exist", "Hello"]);
    assert!(
        combined.contains("not found")
            || combined.contains("Not found")
            || combined.contains("error"),
        "Should error for nonexistent task, got: {}",
        combined
    );
}

#[test]
fn edge_message_to_completed_task() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Complete me", "--id", "done-task"]);
    wg_ok(&wg_dir, &["claim", "done-task"]);
    wg_ok(&wg_dir, &["done", "done-task"]);

    // Verify task is done
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("done-task").unwrap().status, Status::Done);

    // Sending a message to a completed task: the design doc says it's allowed
    // (messages append regardless of status). Verify it works.
    let output = wg_ok(
        &wg_dir,
        &["msg", "send", "done-task", "Post-completion note"],
    );
    assert!(
        output.contains("#1"),
        "Should accept message to completed task"
    );

    // Verify message is stored
    let msgs = messages::list_messages(&wg_dir, "done-task").unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].body, "Post-completion note");
}

#[test]
fn edge_agent_dies_before_reading_messages_persist_for_retry() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Die before read", "--id", "die-1"]);

    // Send messages
    messages::send_message(&wg_dir, "die-1", "Important context", "user", "normal").unwrap();
    messages::send_message(&wg_dir, "die-1", "More context", "coordinator", "normal").unwrap();

    // Simulate agent-1 spawning and dying without reading (cursor never set)
    // Agent-1 cursor is at 0 (default, never read)

    // New agent-2 spawns for retry: should see all messages
    let unread = messages::read_unread(&wg_dir, "die-1", "agent-2").unwrap();
    assert_eq!(
        unread.len(),
        2,
        "New agent should see all messages from failed agent"
    );
    assert_eq!(unread[0].body, "Important context");
    assert_eq!(unread[1].body, "More context");
}

#[test]
fn edge_agent_dies_after_partial_read() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Partial read", "--id", "partial-1"]);

    messages::send_message(&wg_dir, "partial-1", "Msg 1", "user", "normal").unwrap();
    messages::send_message(&wg_dir, "partial-1", "Msg 2", "user", "normal").unwrap();

    // agent-old reads messages (cursor advances to 2)
    let _ = messages::read_unread(&wg_dir, "partial-1", "agent-old").unwrap();

    // More messages arrive after agent-old read but before it died
    messages::send_message(&wg_dir, "partial-1", "Msg 3", "user", "normal").unwrap();

    // agent-old dies. New agent-new spawns. Fresh cursor (0), sees everything.
    let unread = messages::read_unread(&wg_dir, "partial-1", "agent-new").unwrap();
    assert_eq!(
        unread.len(),
        3,
        "New agent gets all messages including those the old agent already read"
    );
}

#[test]
fn edge_multiple_messages_rapid_succession_all_delivered_in_order() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Rapid fire", "--id", "rapid-1"]);

    // Send 20 messages as fast as possible
    for i in 1..=20 {
        messages::send_message(
            &wg_dir,
            "rapid-1",
            &format!("Rapid {}", i),
            "user",
            "normal",
        )
        .unwrap();
    }

    let msgs = messages::list_messages(&wg_dir, "rapid-1").unwrap();
    assert_eq!(msgs.len(), 20, "All 20 messages should be stored");

    // Verify ordering
    for (i, msg) in msgs.iter().enumerate() {
        assert_eq!(msg.id, (i + 1) as u64);
        assert_eq!(msg.body, format!("Rapid {}", i + 1));
    }

    // Verify read_unread returns them all in order
    let unread = messages::read_unread(&wg_dir, "rapid-1", "fast-reader").unwrap();
    assert_eq!(unread.len(), 20);
    for (i, msg) in unread.iter().enumerate() {
        assert_eq!(msg.body, format!("Rapid {}", i + 1));
    }
}

#[test]
fn edge_empty_message_body_rejected_by_cli() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Empty body", "--id", "empty-body"]);

    // Empty string should be rejected
    let combined = wg_fail(&wg_dir, &["msg", "send", "empty-body", ""]);
    assert!(
        combined.contains("empty") || combined.contains("Empty"),
        "Should reject empty message body, got: {}",
        combined
    );
}

#[test]
fn edge_empty_queue_list_returns_empty() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Empty queue", "--id", "empty-q"]);

    let msgs = messages::list_messages(&wg_dir, "empty-q").unwrap();
    assert!(msgs.is_empty());
}

#[test]
fn edge_cli_msg_list_nonexistent_task() {
    let (_tmp, wg_dir) = init_wg();
    let combined = wg_fail(&wg_dir, &["msg", "list", "no-such-task"]);
    assert!(
        combined.contains("not found")
            || combined.contains("Not found")
            || combined.contains("error"),
        "Should error for nonexistent task, got: {}",
        combined
    );
}

#[test]
fn edge_cli_msg_read_nonexistent_task() {
    let (_tmp, wg_dir) = init_wg();
    let combined = wg_fail(&wg_dir, &["msg", "read", "no-such-task", "--agent", "x"]);
    assert!(
        combined.contains("not found")
            || combined.contains("Not found")
            || combined.contains("error"),
        "Should error for nonexistent task, got: {}",
        combined
    );
}

#[test]
fn edge_cli_msg_poll_nonexistent_task() {
    let (_tmp, wg_dir) = init_wg();
    let output = wg_cmd(&wg_dir, &["msg", "poll", "no-such-task", "--agent", "x"]);
    // Should fail (nonexistent task)
    assert!(
        !output.status.success(),
        "Poll for nonexistent task should fail"
    );
}

// ===========================================================================
// 6. COORDINATOR INTEGRATION (message delivery during poll cycle)
// ===========================================================================

#[test]
fn coordinator_deliver_message_stores_and_notifies() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Coord test", "--id", "coord-1"]);

    let agent = workgraph::service::registry::AgentEntry {
        id: "coord-agent".to_string(),
        pid: 55555,
        task_id: "coord-1".to_string(),
        executor: "claude".to_string(),
        started_at: "2026-02-28T00:00:00Z".to_string(),
        last_heartbeat: "2026-02-28T00:00:00Z".to_string(),
        status: workgraph::service::registry::AgentStatus::Working,
        output_file: "/tmp/output.log".to_string(),
        model: None,
        completed_at: None,
        worktree_path: None,
    };

    // Coordinator delivers a message
    let (msg_id, _delivered) = messages::deliver_message(
        &wg_dir,
        "coord-1",
        &agent,
        "Dependency completed",
        "coordinator",
        "normal",
    )
    .unwrap();
    assert_eq!(msg_id, 1);

    // Verify persistent storage
    let msgs = messages::list_messages(&wg_dir, "coord-1").unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].body, "Dependency completed");
    assert_eq!(msgs[0].sender, "coordinator");

    // Verify notification file
    let notif = wg_dir
        .join("agents")
        .join("coord-agent")
        .join("pending_messages.txt");
    assert!(notif.exists());
    let content = fs::read_to_string(&notif).unwrap();
    assert!(content.contains("Dependency completed"));
}

#[test]
fn coordinator_multiple_deliveries_across_tasks() {
    let (_tmp, wg_dir) = init_wg();
    wg_ok(&wg_dir, &["add", "Task A", "--id", "multi-a"]);
    wg_ok(&wg_dir, &["add", "Task B", "--id", "multi-b"]);

    let agent_a = workgraph::service::registry::AgentEntry {
        id: "agent-a".to_string(),
        pid: 11111,
        task_id: "multi-a".to_string(),
        executor: "claude".to_string(),
        started_at: "2026-02-28T00:00:00Z".to_string(),
        last_heartbeat: "2026-02-28T00:00:00Z".to_string(),
        status: workgraph::service::registry::AgentStatus::Working,
        output_file: "/tmp/a.log".to_string(),
        model: None,
        completed_at: None,
        worktree_path: None,
    };

    let agent_b = workgraph::service::registry::AgentEntry {
        id: "agent-b".to_string(),
        pid: 22222,
        task_id: "multi-b".to_string(),
        executor: "amplifier".to_string(),
        started_at: "2026-02-28T00:00:00Z".to_string(),
        last_heartbeat: "2026-02-28T00:00:00Z".to_string(),
        status: workgraph::service::registry::AgentStatus::Working,
        output_file: "/tmp/b.log".to_string(),
        model: None,
        completed_at: None,
        worktree_path: None,
    };

    // Deliver to both tasks
    messages::deliver_message(
        &wg_dir,
        "multi-a",
        &agent_a,
        "For A",
        "coordinator",
        "normal",
    )
    .unwrap();
    messages::deliver_message(
        &wg_dir,
        "multi-b",
        &agent_b,
        "For B",
        "coordinator",
        "urgent",
    )
    .unwrap();

    // Verify separate queues
    let msgs_a = messages::list_messages(&wg_dir, "multi-a").unwrap();
    let msgs_b = messages::list_messages(&wg_dir, "multi-b").unwrap();
    assert_eq!(msgs_a.len(), 1);
    assert_eq!(msgs_b.len(), 1);
    assert_eq!(msgs_a[0].body, "For A");
    assert_eq!(msgs_b[0].body, "For B");

    // Verify separate notification files
    let notif_a = wg_dir
        .join("agents")
        .join("agent-a")
        .join("pending_messages.txt");
    let notif_b = wg_dir
        .join("agents")
        .join("agent-b")
        .join("pending_messages.txt");
    assert!(notif_a.exists());
    assert!(notif_b.exists());
    assert!(fs::read_to_string(&notif_a).unwrap().contains("For A"));
    assert!(fs::read_to_string(&notif_b).unwrap().contains("For B"));
}

// ===========================================================================
// 7. SMOKE TEST: End-to-end flow
// ===========================================================================

/// Full end-to-end: create task → queue messages → claim task → verify messages
/// in context → agent reads messages → agent reads new mid-execution message →
/// complete task. All automated.
#[test]
fn smoke_test_messaging_lifecycle() {
    let (_tmp, wg_dir) = init_wg();

    // Step 1: Create a task
    wg_ok(&wg_dir, &["add", "E2E messaging", "--id", "e2e-msg"]);

    // Step 2: Queue messages BEFORE the task is claimed
    wg_ok(
        &wg_dir,
        &[
            "msg",
            "send",
            "e2e-msg",
            "Pre-claim context: focus on edge cases",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "msg",
            "send",
            "e2e-msg",
            "Urgent: API changed, use v2",
            "--priority",
            "urgent",
            "--from",
            "coordinator",
        ],
    );

    // Step 3: Verify queued messages appear in formatted context
    let formatted = messages::format_queued_messages(&wg_dir, "e2e-msg");
    assert!(formatted.contains("## Queued Messages"));
    assert!(formatted.contains("Pre-claim context"));
    assert!(formatted.contains("Urgent: API changed"));
    assert!(formatted.contains("[URGENT]"));

    // Step 4: Simulate agent spawn (claim + cursor advancement)
    wg_ok(&wg_dir, &["claim", "e2e-msg"]);
    let all_msgs = messages::list_messages(&wg_dir, "e2e-msg").unwrap();
    assert_eq!(all_msgs.len(), 2, "Two pre-claim messages should exist");
    let last_id = all_msgs.last().unwrap().id;
    messages::write_cursor(&wg_dir, "e2e-agent", "e2e-msg", last_id).unwrap();

    // Step 5: Agent reads mid-execution — no new messages yet
    let unread = messages::read_unread(&wg_dir, "e2e-msg", "e2e-agent").unwrap();
    assert!(unread.is_empty(), "No new messages right after spawn");

    // Step 6: Send a mid-execution message (simulating user sending to running agent)
    wg_ok(
        &wg_dir,
        &["msg", "send", "e2e-msg", "Mid-exec: also test logging"],
    );

    // Step 7: Agent polls and sees the new message
    let polled = messages::poll_messages(&wg_dir, "e2e-msg", "e2e-agent").unwrap();
    assert_eq!(polled.len(), 1, "Should see 1 new message");
    assert_eq!(polled[0].body, "Mid-exec: also test logging");

    // Step 8: Agent reads (advances cursor)
    let read = messages::read_unread(&wg_dir, "e2e-msg", "e2e-agent").unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].body, "Mid-exec: also test logging");

    // Step 9: No more unread
    let read2 = messages::read_unread(&wg_dir, "e2e-msg", "e2e-agent").unwrap();
    assert!(read2.is_empty());

    // Step 10: Complete the task
    wg_ok(&wg_dir, &["done", "e2e-msg"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("e2e-msg").unwrap().status, Status::Done);

    // Step 11: All messages are still persisted (3 total)
    let final_msgs = messages::list_messages(&wg_dir, "e2e-msg").unwrap();
    assert_eq!(final_msgs.len(), 3, "All 3 messages should persist");
}

/// Smoke test using CLI commands only (no direct Rust API).
#[test]
fn smoke_test_messaging_cli_only() {
    let (_tmp, wg_dir) = init_wg();

    // Create task
    wg_ok(&wg_dir, &["add", "CLI smoke", "--id", "cli-smoke"]);

    // Send messages via CLI
    wg_ok(&wg_dir, &["msg", "send", "cli-smoke", "CLI message 1"]);
    wg_ok(
        &wg_dir,
        &[
            "msg",
            "send",
            "cli-smoke",
            "CLI message 2",
            "--priority",
            "urgent",
        ],
    );

    // List all messages
    let list_output = wg_ok(&wg_dir, &["msg", "list", "cli-smoke"]);
    assert!(list_output.contains("CLI message 1"));
    assert!(list_output.contains("CLI message 2"));
    assert!(list_output.contains("[URGENT]"));
    assert!(list_output.contains("2 total"));

    // List as JSON
    let json_output = wg_ok(&wg_dir, &["msg", "list", "cli-smoke", "--json"]);
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&json_output).unwrap();
    assert_eq!(parsed.len(), 2);

    // Read (advances cursor for "cli-agent")
    let read_output = wg_ok(
        &wg_dir,
        &["msg", "read", "cli-smoke", "--agent", "cli-agent"],
    );
    assert!(read_output.contains("CLI message 1"));
    assert!(read_output.contains("CLI message 2"));

    // Read again: no unread
    let read_again = wg_ok(
        &wg_dir,
        &["msg", "read", "cli-smoke", "--agent", "cli-agent"],
    );
    assert!(read_again.contains("No unread"));

    // Send new message
    wg_ok(&wg_dir, &["msg", "send", "cli-smoke", "CLI message 3"]);

    // Poll shows new message
    let poll_output = wg_cmd(
        &wg_dir,
        &["msg", "poll", "cli-smoke", "--agent", "cli-agent"],
    );
    assert!(
        poll_output.status.success(),
        "Poll should succeed with new message"
    );
    let stdout = String::from_utf8_lossy(&poll_output.stdout);
    assert!(stdout.contains("CLI message 3"));

    // Read the new message
    let read_new = wg_ok(
        &wg_dir,
        &["msg", "read", "cli-smoke", "--agent", "cli-agent"],
    );
    assert!(read_new.contains("CLI message 3"));
    assert!(
        !read_new.contains("CLI message 1"),
        "Should not re-show old messages"
    );
}

/// Multi-task messaging smoke test.
#[test]
fn smoke_test_multi_task_messaging() {
    let (_tmp, wg_dir) = init_wg();

    wg_ok(&wg_dir, &["add", "Task Alpha", "--id", "alpha"]);
    wg_ok(&wg_dir, &["add", "Task Beta", "--id", "beta"]);

    // Send different messages to different tasks
    wg_ok(&wg_dir, &["msg", "send", "alpha", "Alpha context"]);
    wg_ok(&wg_dir, &["msg", "send", "beta", "Beta context"]);
    wg_ok(&wg_dir, &["msg", "send", "alpha", "More alpha"]);

    // Verify isolation
    let alpha_msgs = messages::list_messages(&wg_dir, "alpha").unwrap();
    let beta_msgs = messages::list_messages(&wg_dir, "beta").unwrap();
    assert_eq!(alpha_msgs.len(), 2, "Alpha should have 2 messages");
    assert_eq!(beta_msgs.len(), 1, "Beta should have 1 message");

    // IDs are independent per task
    assert_eq!(alpha_msgs[0].id, 1);
    assert_eq!(alpha_msgs[1].id, 2);
    assert_eq!(beta_msgs[0].id, 1);
}

// ===========================================================================
// Additional integration: prompt building includes queued messages
// ===========================================================================

#[test]
fn prompt_context_includes_queued_messages_section() {
    use workgraph::context_scope::ContextScope;
    use workgraph::graph::Task;
    use workgraph::service::executor::{ScopeContext, TemplateVars, build_prompt};

    let task = Task {
        id: "test-1".to_string(),
        title: "Test Task".to_string(),
        description: Some("Test description".to_string()),
        ..Task::default()
    };
    let vars = TemplateVars::from_task(&task, None, None);
    let mut ctx = ScopeContext::default();
    ctx.queued_messages = "## Queued Messages\n\n[2026-02-28] user: Focus on testing".to_string();

    let prompt = build_prompt(&vars, ContextScope::Task, &ctx);
    assert!(
        prompt.contains("## Queued Messages"),
        "Prompt should include queued messages section"
    );
    assert!(
        prompt.contains("Focus on testing"),
        "Prompt should include message content"
    );
}

#[test]
fn prompt_context_excludes_empty_queued_messages() {
    use workgraph::context_scope::ContextScope;
    use workgraph::graph::Task;
    use workgraph::service::executor::{ScopeContext, TemplateVars, build_prompt};

    let task = Task {
        id: "test-2".to_string(),
        title: "Test Task".to_string(),
        description: Some("Test description".to_string()),
        ..Task::default()
    };
    let vars = TemplateVars::from_task(&task, None, None);
    let ctx = ScopeContext::default();

    let prompt = build_prompt(&vars, ContextScope::Task, &ctx);
    assert!(
        !prompt.contains("Queued Messages"),
        "Prompt should NOT include queued messages section when empty"
    );
}
