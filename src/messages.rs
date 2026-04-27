//! Message queue storage for inter-agent and user-to-agent communication.
//!
//! Messages are stored as JSONL files in `.workgraph/messages/{task-id}.jsonl`.
//! Read cursors are stored in `.workgraph/messages/.cursors/{agent-id}.{task-id}`.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Delivery status of a message through its lifecycle.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DeliveryStatus {
    /// Message created in queue, waiting for delivery.
    #[default]
    Sent,
    /// Message was included in an agent's prompt (pre-spawn only).
    Delivered,
    /// Agent ran `wg msg read` and this message was returned.
    Read,
    /// Agent explicitly replied to/acknowledged this message.
    Acknowledged,
}

impl std::fmt::Display for DeliveryStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeliveryStatus::Sent => write!(f, "sent"),
            DeliveryStatus::Delivered => write!(f, "delivered"),
            DeliveryStatus::Read => write!(f, "read"),
            DeliveryStatus::Acknowledged => write!(f, "acknowledged"),
        }
    }
}

/// A single message in the queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    /// Unique message ID (monotonic counter per task)
    pub id: u64,
    /// ISO 8601 timestamp
    pub timestamp: String,
    /// Sender identifier: "user", "coordinator", agent-id, or task-id
    pub sender: String,
    /// Message body (free-form text, may contain markdown)
    pub body: String,
    /// Priority: "normal" (default) or "urgent"
    #[serde(default = "default_priority")]
    pub priority: String,
    /// Delivery status tracking.
    #[serde(default)]
    pub status: DeliveryStatus,
    /// ISO 8601 timestamp of when the message was read by the agent.
    /// Set when `update_message_statuses()` transitions status to `Read`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at: Option<String>,
}

fn default_priority() -> String {
    "normal".to_string()
}

/// Directory for message JSONL files.
fn messages_dir(workgraph_dir: &Path) -> PathBuf {
    workgraph_dir.join("messages")
}

/// Path to the JSONL file for a given task.
fn message_file(workgraph_dir: &Path, task_id: &str) -> PathBuf {
    messages_dir(workgraph_dir).join(format!("{}.jsonl", task_id))
}

/// Directory for read cursors.
fn cursors_dir(workgraph_dir: &Path) -> PathBuf {
    messages_dir(workgraph_dir).join(".cursors")
}

/// Path to a cursor file for a given agent + task combination.
fn cursor_file(workgraph_dir: &Path, agent_id: &str, task_id: &str) -> PathBuf {
    cursors_dir(workgraph_dir).join(format!("{}.{}", agent_id, task_id))
}

/// Send a message to a task's queue.
///
/// Appends a new message to `.workgraph/messages/{task-id}.jsonl`.
/// Uses file locking (flock) to safely assign the next message ID.
/// Returns the assigned message ID.
pub fn send_message(
    workgraph_dir: &Path,
    task_id: &str,
    body: &str,
    sender: &str,
    priority: &str,
) -> Result<u64> {
    let msg_dir = messages_dir(workgraph_dir);
    fs::create_dir_all(&msg_dir)
        .with_context(|| format!("Failed to create messages directory: {}", msg_dir.display()))?;

    let path = message_file(workgraph_dir, task_id);

    // Open (or create) the file for read+append with locking
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("Failed to open message file: {}", path.display()))?;

    // Lock the file exclusively for ID assignment + append
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX) };
        if ret != 0 {
            anyhow::bail!(
                "Failed to acquire lock on message file: {}",
                std::io::Error::last_os_error()
            );
        }
    }

    // Read existing messages to find the max ID
    let max_id = {
        let reader = BufReader::new(&file);
        let mut max = 0u64;
        for line in reader.lines() {
            let line = line.context("Failed to read message file line")?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(msg) = serde_json::from_str::<Message>(&line)
                && msg.id > max
            {
                max = msg.id;
            }
        }
        max
    };

    let next_id = max_id + 1;
    let msg = Message {
        id: next_id,
        timestamp: Utc::now().to_rfc3339(),
        sender: sender.to_string(),
        body: body.to_string(),
        priority: priority.to_string(),
        status: DeliveryStatus::Sent,
        read_at: None,
    };

    // Append the message as a single JSON line
    let mut json = serde_json::to_string(&msg).context("Failed to serialize message")?;
    json.push('\n');

    // Write using the file handle (already in append mode)
    let mut file_ref = &file;
    file_ref
        .write_all(json.as_bytes())
        .with_context(|| format!("Failed to write to message file: {}", path.display()))?;

    // Lock is released when file is dropped

    Ok(next_id)
}

/// Count the number of messages for a task (without parsing them).
///
/// Returns 0 if no message file exists for the task.
pub fn message_count(workgraph_dir: &Path, task_id: &str) -> usize {
    let path = message_file(workgraph_dir, task_id);
    if !path.exists() {
        return 0;
    }
    match std::fs::File::open(&path) {
        Ok(file) => {
            let reader = BufReader::new(file);
            reader
                .lines()
                .filter(|line| line.as_ref().map(|l| !l.trim().is_empty()).unwrap_or(false))
                .count()
        }
        Err(_) => 0,
    }
}

/// Per-task message statistics for the viz indicator.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageStats {
    /// Messages sent TO the task (by someone other than the assigned agent).
    pub incoming: usize,
    /// Messages sent BY the task's assigned agent.
    pub outgoing: usize,
    /// Whether the assigned agent has unread messages.
    pub has_unread: bool,
    /// Whether the assigned agent has responded after the latest incoming message.
    pub responded: bool,
}

/// Compute message statistics for a task.
///
/// Determines in/out counts relative to the task's assigned agent,
/// and whether there are unread messages based on the agent's cursor.
pub fn message_stats(
    workgraph_dir: &Path,
    task_id: &str,
    assigned_agent: Option<&str>,
) -> MessageStats {
    let messages = match list_messages(workgraph_dir, task_id) {
        Ok(msgs) => msgs,
        Err(_) => return MessageStats::default(),
    };

    if messages.is_empty() {
        return MessageStats::default();
    }

    let mut incoming = 0usize;
    let mut outgoing = 0usize;
    let mut last_incoming_id: u64 = 0;
    let mut last_outgoing_id: u64 = 0;

    for msg in &messages {
        let is_from_agent = assigned_agent.map(|a| msg.sender == a).unwrap_or(false);
        if is_from_agent {
            outgoing += 1;
            last_outgoing_id = msg.id;
        } else {
            incoming += 1;
            last_incoming_id = msg.id;
        }
    }

    // Check read status: if the assigned agent has a cursor, compare it to max message ID
    let max_id = messages.last().map(|m| m.id).unwrap_or(0);
    let has_unread = if let Some(agent_id) = assigned_agent {
        let cursor = read_cursor(workgraph_dir, agent_id, task_id).unwrap_or(0);
        cursor < max_id
    } else {
        // No assigned agent — treat all messages as unread
        true
    };

    // "Responded" means the agent's last outgoing message is after the last incoming message
    let responded = last_outgoing_id > 0 && last_outgoing_id > last_incoming_id;

    MessageStats {
        incoming,
        outgoing,
        has_unread,
        responded,
    }
}

/// Read all messages for a task, ordered by ID.
pub fn list_messages(workgraph_dir: &Path, task_id: &str) -> Result<Vec<Message>> {
    let path = message_file(workgraph_dir, task_id);
    if !path.exists() {
        return Ok(vec![]);
    }

    let file = fs::File::open(&path)
        .with_context(|| format!("Failed to open message file: {}", path.display()))?;

    let reader = BufReader::new(file);
    let mut messages = Vec::new();

    for line in reader.lines() {
        let line = line.context("Failed to read message file line")?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Message = serde_json::from_str(&line)
            .with_context(|| format!("Failed to parse message: {}", line))?;
        messages.push(msg);
    }

    messages.sort_by_key(|m| m.id);
    Ok(messages)
}

/// Update the delivery status of specific messages in a task's JSONL file.
///
/// Rewrites the file with updated statuses for messages whose IDs are in `ids`.
/// Only upgrades status (sent → delivered → read → acknowledged), never downgrades.
pub fn update_message_statuses(
    workgraph_dir: &Path,
    task_id: &str,
    ids: &[u64],
    new_status: DeliveryStatus,
) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }

    let path = message_file(workgraph_dir, task_id);
    if !path.exists() {
        return Ok(());
    }

    let id_set: std::collections::HashSet<u64> = ids.iter().copied().collect();

    // Read all messages
    let mut messages = list_messages(workgraph_dir, task_id)?;
    let mut changed = false;

    let now = Utc::now().to_rfc3339();
    for msg in &mut messages {
        if id_set.contains(&msg.id) && status_rank(&new_status) > status_rank(&msg.status) {
            if new_status == DeliveryStatus::Read && msg.read_at.is_none() {
                msg.read_at = Some(now.clone());
            }
            msg.status = new_status.clone();
            changed = true;
        }
    }

    if !changed {
        return Ok(());
    }

    // Rewrite the file atomically
    let tmp_path = path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp_path).with_context(|| {
            format!("Failed to create temp message file: {}", tmp_path.display())
        })?;
        for msg in &messages {
            let mut json = serde_json::to_string(msg).context("Failed to serialize message")?;
            json.push('\n');
            file.write_all(json.as_bytes())?;
        }
    }
    fs::rename(&tmp_path, &path)
        .with_context(|| format!("Failed to rename message file: {}", path.display()))?;

    Ok(())
}

/// Numeric rank for status ordering (higher = further along lifecycle).
fn status_rank(status: &DeliveryStatus) -> u8 {
    match status {
        DeliveryStatus::Sent => 0,
        DeliveryStatus::Delivered => 1,
        DeliveryStatus::Read => 2,
        DeliveryStatus::Acknowledged => 3,
    }
}

/// Read the cursor (last-read message ID) for an agent on a task.
///
/// Returns 0 if no cursor exists (meaning all messages are unread).
pub fn read_cursor(workgraph_dir: &Path, agent_id: &str, task_id: &str) -> Result<u64> {
    let path = cursor_file(workgraph_dir, agent_id, task_id);
    if !path.exists() {
        return Ok(0);
    }

    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read cursor file: {}", path.display()))?;

    content.trim().parse::<u64>().with_context(|| {
        format!(
            "Invalid cursor value in {}: '{}'",
            path.display(),
            content.trim()
        )
    })
}

/// Update the cursor for an agent on a task.
///
/// Uses write-to-temp + rename for atomicity.
pub fn write_cursor(
    workgraph_dir: &Path,
    agent_id: &str,
    task_id: &str,
    cursor: u64,
) -> Result<()> {
    let dir = cursors_dir(workgraph_dir);
    fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create cursors directory: {}", dir.display()))?;

    let path = cursor_file(workgraph_dir, agent_id, task_id);
    let tmp_path = path.with_extension("tmp");

    fs::write(&tmp_path, format!("{}\n", cursor))
        .with_context(|| format!("Failed to write temp cursor file: {}", tmp_path.display()))?;

    fs::rename(&tmp_path, &path)
        .with_context(|| format!("Failed to rename cursor file: {}", path.display()))?;

    Ok(())
}

/// Read unread messages for an agent on a task.
///
/// Returns messages with ID > cursor, updates the cursor to the max ID seen,
/// and marks returned messages as "read" (delivery status).
pub fn read_unread(workgraph_dir: &Path, task_id: &str, agent_id: &str) -> Result<Vec<Message>> {
    let cursor = read_cursor(workgraph_dir, agent_id, task_id)?;
    let all = list_messages(workgraph_dir, task_id)?;

    let unread: Vec<Message> = all.into_iter().filter(|m| m.id > cursor).collect();

    if let Some(last) = unread.last() {
        write_cursor(workgraph_dir, agent_id, task_id, last.id)?;

        // Mark all returned messages as read.
        let ids: Vec<u64> = unread.iter().map(|m| m.id).collect();
        let _ = update_message_statuses(workgraph_dir, task_id, &ids, DeliveryStatus::Read);
    }

    Ok(unread)
}

/// Poll for new messages (like read_unread but doesn't advance cursor).
///
/// Returns Ok(messages) where messages may be empty.
pub fn poll_messages(workgraph_dir: &Path, task_id: &str, agent_id: &str) -> Result<Vec<Message>> {
    let cursor = read_cursor(workgraph_dir, agent_id, task_id)?;
    let all = list_messages(workgraph_dir, task_id)?;

    let new: Vec<Message> = all.into_iter().filter(|m| m.id > cursor).collect();
    Ok(new)
}

/// Format a single message for notification files.
fn format_notification_line(msg: &Message) -> String {
    let priority_marker = if msg.priority == "urgent" {
        " [URGENT]"
    } else {
        ""
    };
    format!(
        "[{}] {}{}: {}",
        msg.timestamp, msg.sender, priority_marker, msg.body
    )
}

/// Format queued messages for inclusion in a prompt context.
///
/// Returns an empty string if there are no messages.
/// Marks all formatted messages as "delivered" since they are being embedded in the prompt.
pub fn format_queued_messages(workgraph_dir: &Path, task_id: &str) -> String {
    let messages = match list_messages(workgraph_dir, task_id) {
        Ok(msgs) => msgs,
        Err(_) => return String::new(),
    };

    if messages.is_empty() {
        return String::new();
    }

    // Mark all messages as delivered (included in agent's prompt).
    let ids: Vec<u64> = messages.iter().map(|m| m.id).collect();
    let _ = update_message_statuses(workgraph_dir, task_id, &ids, DeliveryStatus::Delivered);

    let mut lines = vec![
        "## Queued Messages\n\nThe following messages were sent to this task before you started:\n"
            .to_string(),
    ];

    for msg in &messages {
        let priority_marker = if msg.priority == "urgent" {
            " [URGENT]"
        } else {
            ""
        };
        lines.push(format!(
            "[{}] {}{}: {}",
            msg.timestamp, msg.sender, priority_marker, msg.body
        ));
    }

    lines.join("\n")
}

// --- Executor message adapters ---

use crate::service::registry::AgentEntry;

/// Defines how an executor delivers messages to a running agent.
///
/// Each executor type (claude, amplifier, shell) has different capabilities
/// for mid-session message injection. The adapter abstracts these differences.
pub trait MessageAdapter: Send + Sync {
    /// Deliver a message to a running agent.
    ///
    /// Returns `Ok(true)` if the message was delivered (or queued for delivery),
    /// `Ok(false)` if the agent can't receive messages right now,
    /// `Err` if delivery failed due to an error.
    fn deliver(&self, workgraph_dir: &Path, agent: &AgentEntry, message: &Message) -> Result<bool>;

    /// Whether this adapter supports real-time injection (vs polling).
    ///
    /// When false, messages accumulate in the queue and the agent must
    /// poll using `wg msg read` or `wg msg poll`.
    fn supports_realtime(&self) -> bool;

    /// Executor type name (e.g. "claude", "amplifier", "shell").
    fn executor_type(&self) -> &str;
}

/// Notification file path for an agent within its output directory.
///
/// Messages are appended here so agents can detect new messages by
/// checking this file, even if they can't receive real-time injection.
fn notification_file(workgraph_dir: &Path, agent_id: &str) -> PathBuf {
    workgraph_dir
        .join("agents")
        .join(agent_id)
        .join("pending_messages.txt")
}

/// Write a message notification to an agent's notification file.
///
/// This is a best-effort delivery mechanism: append a human-readable
/// line to the agent's notification file so it can detect new messages.
fn write_notification(workgraph_dir: &Path, agent_id: &str, message: &Message) -> Result<()> {
    let path = notification_file(workgraph_dir, agent_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create notification directory: {}",
                parent.display()
            )
        })?;
    }

    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .with_context(|| format!("Failed to open notification file: {}", path.display()))?;

    let line = format!("{}\n", format_notification_line(message));
    file.write_all(line.as_bytes())
        .with_context(|| format!("Failed to write notification: {}", path.display()))?;

    Ok(())
}

/// Claude executor message adapter.
///
/// Claude agents run with `claude --print` which reads stdin once and
/// processes a single turn. Mid-session injection is not supported in v1.
/// Messages accumulate in the queue and are written to a notification file.
/// Agents can self-poll using `wg msg poll`.
pub struct ClaudeMessageAdapter;

impl MessageAdapter for ClaudeMessageAdapter {
    fn deliver(&self, workgraph_dir: &Path, agent: &AgentEntry, message: &Message) -> Result<bool> {
        // Write notification file so the wrapper script or agent can detect it
        write_notification(workgraph_dir, &agent.id, message)?;
        // Can't inject into running claude --print session
        Ok(false)
    }

    fn supports_realtime(&self) -> bool {
        false
    }

    fn executor_type(&self) -> &str {
        "claude"
    }
}

/// Codex executor message adapter.
///
/// Codex runs non-interactively via `codex exec`, so message delivery matches
/// the Claude adapter in v1: queue the message, write a notification file, and
/// let the next spawned turn consume it.
pub struct CodexMessageAdapter;

impl MessageAdapter for CodexMessageAdapter {
    fn deliver(&self, workgraph_dir: &Path, agent: &AgentEntry, message: &Message) -> Result<bool> {
        write_notification(workgraph_dir, &agent.id, message)?;
        Ok(false)
    }

    fn supports_realtime(&self) -> bool {
        false
    }

    fn executor_type(&self) -> &str {
        "codex"
    }
}

/// Amplifier executor message adapter.
///
/// Amplifier runs in `--mode single` with text output. Like the Claude adapter,
/// mid-session injection is not supported in v1. Messages accumulate in the queue
/// and are written to a notification file. The agent can self-poll using
/// `wg msg poll` or `wg msg read`.
///
/// When spawning a new Amplifier agent, queued messages are included in the
/// initial prompt context via `ScopeContext::queued_messages` (handled by
/// `build_scope_context` in `src/commands/spawn/context.rs`).
pub struct AmplifierMessageAdapter;

impl MessageAdapter for AmplifierMessageAdapter {
    fn deliver(&self, workgraph_dir: &Path, agent: &AgentEntry, message: &Message) -> Result<bool> {
        // Write notification file for the agent to detect
        write_notification(workgraph_dir, &agent.id, message)?;
        // Amplifier --mode single doesn't support mid-session injection
        Ok(false)
    }

    fn supports_realtime(&self) -> bool {
        false
    }

    fn executor_type(&self) -> &str {
        "amplifier"
    }
}

/// Shell executor message adapter.
///
/// Shell tasks run arbitrary commands and can call `wg msg read` themselves.
/// The adapter writes a notification file and sets `$WG_MSG_FILE` in the
/// agent's environment at spawn time (handled by the executor config).
pub struct ShellMessageAdapter;

impl MessageAdapter for ShellMessageAdapter {
    fn deliver(&self, workgraph_dir: &Path, agent: &AgentEntry, message: &Message) -> Result<bool> {
        // Write notification file
        write_notification(workgraph_dir, &agent.id, message)?;
        // Shell agents must poll themselves
        Ok(false)
    }

    fn supports_realtime(&self) -> bool {
        false
    }

    fn executor_type(&self) -> &str {
        "shell"
    }
}

/// Status of coordinator-side (TUI) message read state for a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinatorMessageStatus {
    /// Task has messages and the TUI has not viewed them yet.
    Unseen,
    /// TUI has viewed all messages but not replied after the last incoming message.
    Seen,
    /// The TUI (or user/coordinator) has sent a reply after the last incoming message.
    Replied,
}

impl CoordinatorMessageStatus {
    /// ANSI color for this status (ratatui Color).
    pub fn color(&self) -> ratatui::style::Color {
        match self {
            CoordinatorMessageStatus::Unseen => ratatui::style::Color::DarkGray,
            CoordinatorMessageStatus::Seen => ratatui::style::Color::Yellow,
            CoordinatorMessageStatus::Replied => ratatui::style::Color::Green,
        }
    }

    /// Single-character icon for this status.
    pub fn icon(&self) -> char {
        match self {
            CoordinatorMessageStatus::Unseen => '✉',
            CoordinatorMessageStatus::Seen => '↩',
            CoordinatorMessageStatus::Replied => '✓',
        }
    }

    /// ANSI escape code prefix for terminal coloring.
    pub fn ansi_prefix(&self) -> &'static str {
        match self {
            CoordinatorMessageStatus::Unseen => "\x1b[90m", // DarkGray
            CoordinatorMessageStatus::Seen => "\x1b[33m",   // Yellow
            CoordinatorMessageStatus::Replied => "\x1b[32m", // Green
        }
    }
}

/// Compute the coordinator (TUI) message status for a given task.
///
/// Uses the "tui" cursor to determine what the coordinator has seen.
/// - `Replied`: an outgoing message from "tui", "user", or "coordinator" exists
///   after the last incoming message.
/// - `Seen`: tui cursor >= max incoming message ID (all seen, no reply yet).
/// - `Unseen`: tui cursor < max incoming message ID or no cursor exists.
///
/// Returns `None` if the task has no messages.
pub fn coordinator_message_status(
    workgraph_dir: &Path,
    task_id: &str,
) -> Option<CoordinatorMessageStatus> {
    let messages = list_messages(workgraph_dir, task_id).ok()?;
    if messages.is_empty() {
        return None;
    }

    // Senders considered "coordinator-side" (outgoing from the TUI perspective).
    let is_coordinator_sender = |sender: &str| matches!(sender, "tui" | "user" | "coordinator");

    let mut last_incoming_id: u64 = 0;
    let mut last_outgoing_after_incoming_id: u64 = 0;

    // Walk messages in order to find last incoming and whether there's an outgoing reply after it.
    for msg in &messages {
        if is_coordinator_sender(&msg.sender) {
            // This is an outgoing message from the coordinator side.
            if last_incoming_id > 0 && msg.id > last_incoming_id {
                last_outgoing_after_incoming_id = msg.id;
            }
        } else {
            // Incoming message — reset the outgoing-after-incoming tracker.
            last_incoming_id = msg.id;
            last_outgoing_after_incoming_id = 0;
        }
    }

    if last_incoming_id == 0 {
        // All messages are outgoing — no incoming messages to respond to.
        return None;
    }

    // Check if there's a reply after the last incoming message.
    if last_outgoing_after_incoming_id > last_incoming_id {
        return Some(CoordinatorMessageStatus::Replied);
    }

    // Check the TUI cursor: if it's >= last_incoming_id, the user has seen everything.
    let tui_cursor = read_cursor(workgraph_dir, "tui", task_id).unwrap_or(0);
    if tui_cursor >= last_incoming_id {
        Some(CoordinatorMessageStatus::Seen)
    } else {
        Some(CoordinatorMessageStatus::Unseen)
    }
}

/// Create the appropriate message adapter for a given executor type.
///
/// Returns a boxed trait object that handles message delivery for that executor.
pub fn adapter_for_executor(executor_type: &str) -> Box<dyn MessageAdapter> {
    match executor_type {
        "claude" => Box::new(ClaudeMessageAdapter),
        "codex" => Box::new(CodexMessageAdapter),
        "amplifier" => Box::new(AmplifierMessageAdapter),
        "shell" => Box::new(ShellMessageAdapter),
        // Default to claude-like behavior for unknown executors
        _ => Box::new(ClaudeMessageAdapter),
    }
}

/// Deliver a message to a running agent using the appropriate adapter.
///
/// This is the main entry point for message delivery. It:
/// 1. Sends the message to the task's queue (persistent storage)
/// 2. Attempts real-time delivery via the executor adapter
///
/// Returns the message ID and whether real-time delivery succeeded.
pub fn deliver_message(
    workgraph_dir: &Path,
    task_id: &str,
    agent: &AgentEntry,
    body: &str,
    sender: &str,
    priority: &str,
) -> Result<(u64, bool)> {
    // 1. Store in persistent queue
    let msg_id = send_message(workgraph_dir, task_id, body, sender, priority)?;

    // 2. Try real-time delivery via adapter
    let adapter = adapter_for_executor(&agent.executor);
    let msg = Message {
        id: msg_id,
        timestamp: Utc::now().to_rfc3339(),
        sender: sender.to_string(),
        body: body.to_string(),
        priority: priority.to_string(),
        status: DeliveryStatus::Sent,
        read_at: None,
    };
    let delivered = adapter.deliver(workgraph_dir, agent, &msg)?;

    Ok((msg_id, delivered))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();
        (tmp, wg_dir)
    }

    #[test]
    fn test_send_and_list_messages() {
        let (_tmp, wg_dir) = setup();

        let id1 = send_message(&wg_dir, "task-1", "Hello", "user", "normal").unwrap();
        assert_eq!(id1, 1);

        let id2 = send_message(&wg_dir, "task-1", "World", "coordinator", "urgent").unwrap();
        assert_eq!(id2, 2);

        let msgs = list_messages(&wg_dir, "task-1").unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].id, 1);
        assert_eq!(msgs[0].body, "Hello");
        assert_eq!(msgs[0].sender, "user");
        assert_eq!(msgs[0].priority, "normal");
        assert_eq!(msgs[1].id, 2);
        assert_eq!(msgs[1].body, "World");
        assert_eq!(msgs[1].sender, "coordinator");
        assert_eq!(msgs[1].priority, "urgent");
    }

    #[test]
    fn test_list_empty() {
        let (_tmp, wg_dir) = setup();

        let msgs = list_messages(&wg_dir, "nonexistent").unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn test_message_count() {
        let (_tmp, wg_dir) = setup();

        assert_eq!(message_count(&wg_dir, "task-1"), 0);

        send_message(&wg_dir, "task-1", "First", "user", "normal").unwrap();
        assert_eq!(message_count(&wg_dir, "task-1"), 1);

        send_message(&wg_dir, "task-1", "Second", "user", "normal").unwrap();
        send_message(&wg_dir, "task-1", "Third", "coordinator", "urgent").unwrap();
        assert_eq!(message_count(&wg_dir, "task-1"), 3);

        // Different task has separate count
        assert_eq!(message_count(&wg_dir, "task-2"), 0);
    }

    #[test]
    fn test_message_stats_empty() {
        let (_tmp, wg_dir) = setup();

        let stats = message_stats(&wg_dir, "task-1", Some("agent-1"));
        assert_eq!(stats.incoming, 0);
        assert_eq!(stats.outgoing, 0);
        assert!(!stats.has_unread);
        assert!(!stats.responded);
    }

    #[test]
    fn test_message_stats_incoming_only() {
        let (_tmp, wg_dir) = setup();

        send_message(&wg_dir, "task-1", "Hello", "user", "normal").unwrap();
        send_message(&wg_dir, "task-1", "Update", "coordinator", "normal").unwrap();

        let stats = message_stats(&wg_dir, "task-1", Some("agent-1"));
        assert_eq!(stats.incoming, 2);
        assert_eq!(stats.outgoing, 0);
        assert!(stats.has_unread);
        assert!(!stats.responded);
    }

    #[test]
    fn test_message_stats_with_outgoing() {
        let (_tmp, wg_dir) = setup();

        send_message(&wg_dir, "task-1", "Hello", "user", "normal").unwrap();
        send_message(&wg_dir, "task-1", "Reply", "agent-1", "normal").unwrap();

        let stats = message_stats(&wg_dir, "task-1", Some("agent-1"));
        assert_eq!(stats.incoming, 1);
        assert_eq!(stats.outgoing, 1);
        assert!(stats.has_unread); // cursor not advanced
        assert!(stats.responded); // last message is outgoing
    }

    #[test]
    fn test_message_stats_read_status() {
        let (_tmp, wg_dir) = setup();

        send_message(&wg_dir, "task-1", "Hello", "user", "normal").unwrap();
        // Agent reads the message (advance cursor)
        write_cursor(&wg_dir, "agent-1", "task-1", 1).unwrap();

        let stats = message_stats(&wg_dir, "task-1", Some("agent-1"));
        assert_eq!(stats.incoming, 1);
        assert!(!stats.has_unread); // cursor is at max
        assert!(!stats.responded); // no outgoing messages
    }

    #[test]
    fn test_message_stats_responded() {
        let (_tmp, wg_dir) = setup();

        send_message(&wg_dir, "task-1", "Hello", "user", "normal").unwrap();
        send_message(&wg_dir, "task-1", "Reply", "agent-1", "normal").unwrap();
        // Agent reads all messages
        write_cursor(&wg_dir, "agent-1", "task-1", 2).unwrap();

        let stats = message_stats(&wg_dir, "task-1", Some("agent-1"));
        assert!(!stats.has_unread);
        assert!(stats.responded); // last msg is outgoing (id=2 > last incoming id=1)
    }

    #[test]
    fn test_message_stats_no_agent() {
        let (_tmp, wg_dir) = setup();

        send_message(&wg_dir, "task-1", "Hello", "user", "normal").unwrap();

        let stats = message_stats(&wg_dir, "task-1", None);
        assert_eq!(stats.incoming, 1);
        assert_eq!(stats.outgoing, 0);
        assert!(stats.has_unread); // no agent = all unread
    }

    #[test]
    fn test_cursor_roundtrip() {
        let (_tmp, wg_dir) = setup();

        assert_eq!(read_cursor(&wg_dir, "agent-1", "task-1").unwrap(), 0);

        write_cursor(&wg_dir, "agent-1", "task-1", 5).unwrap();
        assert_eq!(read_cursor(&wg_dir, "agent-1", "task-1").unwrap(), 5);

        write_cursor(&wg_dir, "agent-1", "task-1", 10).unwrap();
        assert_eq!(read_cursor(&wg_dir, "agent-1", "task-1").unwrap(), 10);
    }

    #[test]
    fn test_read_unread_advances_cursor() {
        let (_tmp, wg_dir) = setup();

        send_message(&wg_dir, "task-1", "First", "user", "normal").unwrap();
        send_message(&wg_dir, "task-1", "Second", "user", "normal").unwrap();

        // First read: both messages are unread
        let unread = read_unread(&wg_dir, "task-1", "agent-1").unwrap();
        assert_eq!(unread.len(), 2);

        // Second read: no new messages
        let unread = read_unread(&wg_dir, "task-1", "agent-1").unwrap();
        assert!(unread.is_empty());

        // Send a third message
        send_message(&wg_dir, "task-1", "Third", "coordinator", "normal").unwrap();

        // Third read: only the new message
        let unread = read_unread(&wg_dir, "task-1", "agent-1").unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].body, "Third");
    }

    #[test]
    fn test_poll_does_not_advance_cursor() {
        let (_tmp, wg_dir) = setup();

        send_message(&wg_dir, "task-1", "First", "user", "normal").unwrap();

        // Poll returns messages but doesn't advance cursor
        let msgs = poll_messages(&wg_dir, "task-1", "agent-1").unwrap();
        assert_eq!(msgs.len(), 1);

        // Poll again: still returns the same messages
        let msgs = poll_messages(&wg_dir, "task-1", "agent-1").unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn test_read_at_timestamp_set_on_read() {
        let (_tmp, wg_dir) = setup();

        send_message(&wg_dir, "task-1", "Hello", "user", "normal").unwrap();
        send_message(&wg_dir, "task-1", "World", "user", "normal").unwrap();

        // Before reading: read_at should be None.
        let msgs = list_messages(&wg_dir, "task-1").unwrap();
        assert!(msgs[0].read_at.is_none());
        assert!(msgs[1].read_at.is_none());

        // Read the messages as agent-1.
        let unread = read_unread(&wg_dir, "task-1", "agent-1").unwrap();
        assert_eq!(unread.len(), 2);

        // After reading: read_at should be set.
        let msgs = list_messages(&wg_dir, "task-1").unwrap();
        assert!(
            msgs[0].read_at.is_some(),
            "read_at should be set after read_unread"
        );
        assert!(
            msgs[1].read_at.is_some(),
            "read_at should be set after read_unread"
        );

        // The read_at should be a valid RFC 3339 timestamp.
        let read_at = msgs[0].read_at.as_ref().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(read_at).is_ok());
    }

    #[test]
    fn test_separate_cursors_per_agent() {
        let (_tmp, wg_dir) = setup();

        send_message(&wg_dir, "task-1", "Hello", "user", "normal").unwrap();

        // agent-1 reads
        let unread = read_unread(&wg_dir, "task-1", "agent-1").unwrap();
        assert_eq!(unread.len(), 1);

        // agent-2 hasn't read yet
        let unread = read_unread(&wg_dir, "task-1", "agent-2").unwrap();
        assert_eq!(unread.len(), 1);

        // agent-1 has no more unread
        let unread = read_unread(&wg_dir, "task-1", "agent-1").unwrap();
        assert!(unread.is_empty());
    }

    #[test]
    fn test_separate_queues_per_task() {
        let (_tmp, wg_dir) = setup();

        send_message(&wg_dir, "task-1", "For task 1", "user", "normal").unwrap();
        send_message(&wg_dir, "task-2", "For task 2", "user", "normal").unwrap();

        let msgs1 = list_messages(&wg_dir, "task-1").unwrap();
        let msgs2 = list_messages(&wg_dir, "task-2").unwrap();

        assert_eq!(msgs1.len(), 1);
        assert_eq!(msgs1[0].body, "For task 1");
        assert_eq!(msgs2.len(), 1);
        assert_eq!(msgs2[0].body, "For task 2");
    }

    #[test]
    fn test_format_queued_messages_empty() {
        let (_tmp, wg_dir) = setup();

        let formatted = format_queued_messages(&wg_dir, "task-1");
        assert!(formatted.is_empty());
    }

    #[test]
    fn test_format_queued_messages_with_messages() {
        let (_tmp, wg_dir) = setup();

        send_message(
            &wg_dir,
            "task-1",
            "Focus on error handling",
            "user",
            "normal",
        )
        .unwrap();
        send_message(
            &wg_dir,
            "task-1",
            "Urgent fix needed",
            "coordinator",
            "urgent",
        )
        .unwrap();

        let formatted = format_queued_messages(&wg_dir, "task-1");
        assert!(formatted.contains("## Queued Messages"));
        assert!(formatted.contains("Focus on error handling"));
        assert!(formatted.contains("[URGENT]"));
        assert!(formatted.contains("Urgent fix needed"));
    }

    #[test]
    fn test_message_ordering() {
        let (_tmp, wg_dir) = setup();

        // Send messages in order
        for i in 1..=5 {
            send_message(
                &wg_dir,
                "task-1",
                &format!("Message {}", i),
                "user",
                "normal",
            )
            .unwrap();
        }

        let msgs = list_messages(&wg_dir, "task-1").unwrap();
        assert_eq!(msgs.len(), 5);
        for (i, msg) in msgs.iter().enumerate() {
            assert_eq!(msg.id, (i + 1) as u64);
            assert_eq!(msg.body, format!("Message {}", i + 1));
        }
    }

    #[test]
    fn test_message_timestamps_are_valid() {
        let (_tmp, wg_dir) = setup();

        send_message(&wg_dir, "task-1", "Test", "user", "normal").unwrap();

        let msgs = list_messages(&wg_dir, "task-1").unwrap();
        assert_eq!(msgs.len(), 1);
        // Verify timestamp is valid RFC 3339
        chrono::DateTime::parse_from_rfc3339(&msgs[0].timestamp)
            .expect("timestamp should be valid RFC 3339");
    }

    // --- MessageAdapter tests ---

    fn make_agent(id: &str, executor: &str) -> AgentEntry {
        AgentEntry {
            id: id.to_string(),
            pid: 12345,
            task_id: "task-1".to_string(),
            executor: executor.to_string(),
            started_at: "2026-02-28T00:00:00Z".to_string(),
            last_heartbeat: "2026-02-28T00:00:00Z".to_string(),
            status: crate::service::registry::AgentStatus::Working,
            output_file: "/tmp/output.log".to_string(),
            model: None,
            completed_at: None,
            worktree_path: None,
        }
    }

    #[test]
    fn test_adapter_for_executor_claude() {
        let adapter = adapter_for_executor("claude");
        assert_eq!(adapter.executor_type(), "claude");
        assert!(!adapter.supports_realtime());
    }

    #[test]
    fn test_adapter_for_executor_amplifier() {
        let adapter = adapter_for_executor("amplifier");
        assert_eq!(adapter.executor_type(), "amplifier");
        assert!(!adapter.supports_realtime());
    }

    #[test]
    fn test_adapter_for_executor_codex() {
        let adapter = adapter_for_executor("codex");
        assert_eq!(adapter.executor_type(), "codex");
        assert!(!adapter.supports_realtime());
    }

    #[test]
    fn test_adapter_for_executor_shell() {
        let adapter = adapter_for_executor("shell");
        assert_eq!(adapter.executor_type(), "shell");
        assert!(!adapter.supports_realtime());
    }

    #[test]
    fn test_adapter_for_unknown_executor_defaults_to_claude() {
        let adapter = adapter_for_executor("unknown-thing");
        assert_eq!(adapter.executor_type(), "claude");
    }

    #[test]
    fn test_claude_adapter_writes_notification() {
        let (_tmp, wg_dir) = setup();
        let agent = make_agent("agent-1", "claude");

        // Create agent directory
        fs::create_dir_all(wg_dir.join("agents").join("agent-1")).unwrap();

        let adapter = ClaudeMessageAdapter;
        let msg = Message {
            id: 1,
            timestamp: "2026-02-28T00:00:00Z".to_string(),
            sender: "user".to_string(),
            body: "Hello agent".to_string(),
            priority: "normal".to_string(),
            status: DeliveryStatus::Sent,
            read_at: None,
        };

        let delivered = adapter.deliver(&wg_dir, &agent, &msg).unwrap();
        assert!(
            !delivered,
            "Claude adapter should not support realtime delivery"
        );

        // Check notification file was written
        let notif_path = notification_file(&wg_dir, "agent-1");
        assert!(notif_path.exists(), "Notification file should exist");
        let content = fs::read_to_string(&notif_path).unwrap();
        assert!(content.contains("Hello agent"));
        assert!(content.contains("user"));
    }

    #[test]
    fn test_amplifier_adapter_writes_notification() {
        let (_tmp, wg_dir) = setup();
        let agent = make_agent("agent-2", "amplifier");

        // Create agent directory
        fs::create_dir_all(wg_dir.join("agents").join("agent-2")).unwrap();

        let adapter = AmplifierMessageAdapter;
        let msg = Message {
            id: 1,
            timestamp: "2026-02-28T00:00:00Z".to_string(),
            sender: "coordinator".to_string(),
            body: "Context update".to_string(),
            priority: "urgent".to_string(),
            status: DeliveryStatus::Sent,
            read_at: None,
        };

        let delivered = adapter.deliver(&wg_dir, &agent, &msg).unwrap();
        assert!(
            !delivered,
            "Amplifier adapter should not support realtime delivery"
        );

        // Check notification file was written
        let notif_path = notification_file(&wg_dir, "agent-2");
        assert!(notif_path.exists(), "Notification file should exist");
        let content = fs::read_to_string(&notif_path).unwrap();
        assert!(content.contains("Context update"));
        assert!(content.contains("[URGENT]"));
        assert!(content.contains("coordinator"));
    }

    #[test]
    fn test_adapter_notification_accumulates() {
        let (_tmp, wg_dir) = setup();
        let agent = make_agent("agent-3", "amplifier");
        fs::create_dir_all(wg_dir.join("agents").join("agent-3")).unwrap();

        let adapter = AmplifierMessageAdapter;

        // Send multiple messages
        for i in 1..=3 {
            let msg = Message {
                id: i,
                timestamp: format!("2026-02-28T00:00:0{}Z", i),
                sender: "user".to_string(),
                body: format!("Message {}", i),
                priority: "normal".to_string(),
                status: DeliveryStatus::Sent,
                read_at: None,
            };
            adapter.deliver(&wg_dir, &agent, &msg).unwrap();
        }

        let notif_path = notification_file(&wg_dir, "agent-3");
        let content = fs::read_to_string(&notif_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3, "Should have 3 notification lines");
        assert!(lines[0].contains("Message 1"));
        assert!(lines[1].contains("Message 2"));
        assert!(lines[2].contains("Message 3"));
    }

    #[test]
    fn test_deliver_message_stores_and_notifies() {
        let (_tmp, wg_dir) = setup();
        let agent = make_agent("agent-4", "amplifier");
        fs::create_dir_all(wg_dir.join("agents").join("agent-4")).unwrap();

        let (msg_id, delivered) = deliver_message(
            &wg_dir,
            "task-1",
            &agent,
            "Important update",
            "coordinator",
            "urgent",
        )
        .unwrap();

        assert_eq!(msg_id, 1);
        assert!(!delivered, "v1 adapters don't support realtime delivery");

        // Verify message was stored in queue
        let msgs = list_messages(&wg_dir, "task-1").unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].body, "Important update");
        assert_eq!(msgs[0].priority, "urgent");

        // Verify notification was written
        let notif_path = notification_file(&wg_dir, "agent-4");
        assert!(notif_path.exists());
        let content = fs::read_to_string(&notif_path).unwrap();
        assert!(content.contains("Important update"));
    }

    #[test]
    fn test_notification_creates_directory() {
        let (_tmp, wg_dir) = setup();
        // Don't pre-create the agent directory — write_notification should handle it
        let agent = make_agent("agent-new", "claude");

        let msg = Message {
            id: 1,
            timestamp: "2026-02-28T00:00:00Z".to_string(),
            sender: "user".to_string(),
            body: "Auto-create dir".to_string(),
            priority: "normal".to_string(),
            status: DeliveryStatus::Sent,
            read_at: None,
        };

        let adapter = ClaudeMessageAdapter;
        adapter.deliver(&wg_dir, &agent, &msg).unwrap();

        let notif_path = notification_file(&wg_dir, "agent-new");
        assert!(notif_path.exists());
    }

    // --- CoordinatorMessageStatus tests ---

    #[test]
    fn test_coordinator_message_status_no_messages() {
        let (_tmp, wg_dir) = setup();
        let status = coordinator_message_status(&wg_dir, "task-1");
        assert_eq!(status, None);
    }

    #[test]
    fn test_coordinator_message_status_unseen() {
        let (_tmp, wg_dir) = setup();
        // Send an incoming message (from agent, not tui/user/coordinator)
        send_message(&wg_dir, "task-1", "Hello", "agent-1", "normal").unwrap();
        // No cursor set — should be Unseen
        let status = coordinator_message_status(&wg_dir, "task-1");
        assert_eq!(status, Some(CoordinatorMessageStatus::Unseen));
    }

    #[test]
    fn test_coordinator_message_status_unseen_cursor_behind() {
        let (_tmp, wg_dir) = setup();
        send_message(&wg_dir, "task-1", "Msg 1", "agent-1", "normal").unwrap();
        send_message(&wg_dir, "task-1", "Msg 2", "agent-1", "normal").unwrap();
        // Cursor at 1 but last incoming is 2
        write_cursor(&wg_dir, "tui", "task-1", 1).unwrap();
        let status = coordinator_message_status(&wg_dir, "task-1");
        assert_eq!(status, Some(CoordinatorMessageStatus::Unseen));
    }

    #[test]
    fn test_coordinator_message_status_seen() {
        let (_tmp, wg_dir) = setup();
        send_message(&wg_dir, "task-1", "Hello", "agent-1", "normal").unwrap();
        // TUI cursor advanced to cover the incoming message
        write_cursor(&wg_dir, "tui", "task-1", 1).unwrap();
        let status = coordinator_message_status(&wg_dir, "task-1");
        assert_eq!(status, Some(CoordinatorMessageStatus::Seen));
    }

    #[test]
    fn test_coordinator_message_status_replied_from_tui() {
        let (_tmp, wg_dir) = setup();
        send_message(&wg_dir, "task-1", "Question", "agent-1", "normal").unwrap();
        send_message(&wg_dir, "task-1", "Answer", "tui", "normal").unwrap();
        let status = coordinator_message_status(&wg_dir, "task-1");
        assert_eq!(status, Some(CoordinatorMessageStatus::Replied));
    }

    #[test]
    fn test_coordinator_message_status_replied_from_user() {
        let (_tmp, wg_dir) = setup();
        send_message(&wg_dir, "task-1", "Question", "agent-1", "normal").unwrap();
        send_message(&wg_dir, "task-1", "Answer", "user", "normal").unwrap();
        let status = coordinator_message_status(&wg_dir, "task-1");
        assert_eq!(status, Some(CoordinatorMessageStatus::Replied));
    }

    #[test]
    fn test_coordinator_message_status_replied_from_coordinator() {
        let (_tmp, wg_dir) = setup();
        send_message(&wg_dir, "task-1", "Question", "agent-1", "normal").unwrap();
        send_message(&wg_dir, "task-1", "Answer", "coordinator", "normal").unwrap();
        let status = coordinator_message_status(&wg_dir, "task-1");
        assert_eq!(status, Some(CoordinatorMessageStatus::Replied));
    }

    #[test]
    fn test_coordinator_message_status_replied_resets_on_new_incoming() {
        let (_tmp, wg_dir) = setup();
        // Agent asks, user replies, then agent asks again
        send_message(&wg_dir, "task-1", "Q1", "agent-1", "normal").unwrap();
        send_message(&wg_dir, "task-1", "A1", "user", "normal").unwrap();
        send_message(&wg_dir, "task-1", "Q2", "agent-1", "normal").unwrap();
        // Now Q2 is unanswered — should be Unseen (no cursor advance)
        let status = coordinator_message_status(&wg_dir, "task-1");
        assert_eq!(status, Some(CoordinatorMessageStatus::Unseen));
    }

    #[test]
    fn test_coordinator_message_status_all_outgoing_returns_none() {
        let (_tmp, wg_dir) = setup();
        // Only outgoing messages from the coordinator side
        send_message(&wg_dir, "task-1", "Hello", "user", "normal").unwrap();
        send_message(&wg_dir, "task-1", "Follow-up", "tui", "normal").unwrap();
        let status = coordinator_message_status(&wg_dir, "task-1");
        assert_eq!(status, None);
    }
}
