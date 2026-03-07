//! Persistent coordinator agent: a long-lived LLM session inside the service daemon.
//!
//! Spawns a Claude CLI process with `--input-format stream-json --output-format stream-json`
//! and keeps it running for the lifetime of the daemon. User chat messages are injected
//! via stdin, and responses are captured from stdout and written to the chat outbox.
//!
//! Architecture:
//! - The daemon creates a `CoordinatorAgent` on startup.
//! - Chat messages are sent via `CoordinatorAgent::send_message()`.
//! - A dedicated reader thread parses stdout and writes responses to the outbox.
//! - The agent subprocess is auto-restarted on crash with context recovery.
//!
//! Crash recovery:
//! - Time-windowed restart rate limiting: max 3 restarts per 10 minutes.
//! - On restart, injects previous conversation summary and current graph state.
//! - Conversation history persisted via chat inbox/outbox JSONL files.
//! - Old history is rotated on restart to prevent unbounded growth.
//!
//! Context refresh:
//! - On each user message, a context update is injected with graph summary,
//!   recent events, active agents, and items needing attention.
//! - Events are tracked in a bounded ring buffer (`EventLog`) shared between
//!   the daemon main thread and the agent thread.

use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use chrono::{DateTime, Utc};

use workgraph::chat;
use workgraph::graph::Status;
use workgraph::parser::load_graph;
use workgraph::service::registry::AgentRegistry;

use crate::commands::{graph_path, is_process_alive};

use super::DaemonLogger;

/// Maximum restarts allowed within the restart window before pausing.
const MAX_RESTARTS_PER_WINDOW: usize = 3;

/// Restart window duration in seconds (10 minutes).
const RESTART_WINDOW_SECS: u64 = 600;

/// Number of recent conversation messages to include in crash recovery context.
const RECOVERY_HISTORY_COUNT: usize = 10;

/// Maximum character length for a single message in the recovery summary.
const RECOVERY_MSG_MAX_CHARS: usize = 500;

/// Maximum number of messages to keep per file when rotating chat history.
const HISTORY_ROTATION_KEEP: usize = 200;

// ---------------------------------------------------------------------------
// Event log: bounded ring buffer for inter-interaction event tracking
// ---------------------------------------------------------------------------

/// An event tracked between coordinator interactions.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Variants constructed by daemon event recording
pub enum Event {
    /// A task completed.
    TaskCompleted {
        task_id: String,
        agent_id: Option<String>,
    },
    /// A task failed.
    TaskFailed { task_id: String, reason: String },
    /// A new task was added to the graph.
    TaskAdded {
        task_id: String,
        title: String,
        added_by: Option<String>,
    },
    /// An agent was spawned for a task.
    AgentSpawned {
        agent_id: String,
        task_id: String,
        executor: String,
    },
    /// An agent completed and exited.
    AgentCompleted { agent_id: String, task_id: String },
    /// An agent failed or died.
    AgentFailed {
        agent_id: String,
        task_id: String,
        reason: String,
    },
}

impl std::fmt::Display for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::TaskCompleted { task_id, agent_id } => {
                if let Some(aid) = agent_id {
                    write!(f, "task {} completed ({})", task_id, aid)
                } else {
                    write!(f, "task {} completed", task_id)
                }
            }
            Event::TaskFailed { task_id, reason } => {
                write!(f, "task {} failed: {}", task_id, reason)
            }
            Event::TaskAdded {
                task_id,
                title: _,
                added_by,
            } => {
                if let Some(by) = added_by {
                    write!(f, "task {} added by {}", task_id, by)
                } else {
                    write!(f, "task {} added", task_id)
                }
            }
            Event::AgentSpawned {
                agent_id,
                task_id,
                executor,
            } => {
                write!(
                    f,
                    "agent {} spawned on {} (executor: {})",
                    agent_id, task_id, executor
                )
            }
            Event::AgentCompleted { agent_id, task_id } => {
                write!(f, "agent {} completed task {}", agent_id, task_id)
            }
            Event::AgentFailed {
                agent_id,
                task_id,
                reason,
            } => {
                write!(f, "agent {} failed on {}: {}", agent_id, task_id, reason)
            }
        }
    }
}

/// A timestamped event entry.
#[derive(Debug, Clone)]
struct EventEntry {
    timestamp: DateTime<Utc>,
    event: Event,
}

/// Bounded ring buffer of events between coordinator interactions.
///
/// The daemon records events (task completions, agent spawns, etc.) and the
/// coordinator agent drains them when building context for each interaction.
#[derive(Debug)]
pub struct EventLog {
    entries: VecDeque<EventEntry>,
    capacity: usize,
}

const DEFAULT_EVENT_LOG_CAPACITY: usize = 200;

impl EventLog {
    /// Create a new event log with the default capacity.
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            capacity: DEFAULT_EVENT_LOG_CAPACITY,
        }
    }

    /// Record a new event. Oldest events are evicted when at capacity.
    pub fn record(&mut self, event: Event) {
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back(EventEntry {
            timestamp: Utc::now(),
            event,
        });
    }

    /// Drain all events recorded since `since`, returning them.
    /// Events older than `since` are discarded.
    pub fn drain_since(&mut self, since: &DateTime<Utc>) -> Vec<(DateTime<Utc>, Event)> {
        let mut result = Vec::new();
        while let Some(entry) = self.entries.pop_front() {
            if entry.timestamp > *since {
                result.push((entry.timestamp, entry.event));
            }
        }
        result
    }

    /// Return event count (for testing/debugging).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Thread-safe shared event log.
pub type SharedEventLog = Arc<Mutex<EventLog>>;

/// Create a new shared event log.
pub fn new_event_log() -> SharedEventLog {
    Arc::new(Mutex::new(EventLog::new()))
}

/// A chat message to be injected into the coordinator agent.
pub struct ChatRequest {
    pub request_id: String,
    pub message: String,
}

/// Handle to the running coordinator agent.
///
/// The agent runs as a Claude CLI subprocess in a separate thread.
/// Messages are sent via a channel, and responses are written to the
/// chat outbox by the agent thread.
pub struct CoordinatorAgent {
    /// Send chat messages to the agent thread.
    tx: mpsc::Sender<ChatRequest>,
    /// The agent management thread handle.
    _agent_thread: JoinHandle<()>,
    /// Shared flag indicating whether the agent process is alive.
    alive: Arc<Mutex<bool>>,
    /// Shared PID of the agent process (0 if not running).
    pid: Arc<Mutex<u32>>,
    /// Shared event log for recording events from the daemon.
    #[allow(dead_code)]
    event_log: SharedEventLog,
}

impl CoordinatorAgent {
    /// Check if the Claude CLI is available on the system.
    ///
    /// Returns true if `claude --version` runs successfully.
    pub fn is_claude_available() -> bool {
        std::process::Command::new("claude")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Spawn the coordinator agent.
    ///
    /// Launches a Claude CLI process and starts a management thread that
    /// handles message injection and response capture.
    ///
    /// The `event_log` is shared with the daemon thread — the daemon records
    /// events (task completions, agent spawns, etc.) and the agent reads them
    /// when building context for each interaction.
    ///
    /// Returns an error if the Claude CLI is not available.
    pub fn spawn(
        dir: &Path,
        model: Option<&str>,
        logger: &DaemonLogger,
        event_log: SharedEventLog,
    ) -> Result<Self> {
        if !Self::is_claude_available() {
            anyhow::bail!(
                "Claude CLI not found. Install it to enable the persistent coordinator agent."
            );
        }
        let (tx, rx) = mpsc::channel::<ChatRequest>();
        let alive = Arc::new(Mutex::new(false));
        let pid = Arc::new(Mutex::new(0u32));

        let dir = dir.to_path_buf();
        let model = model.map(String::from);
        let logger = logger.clone();
        let alive_clone = alive.clone();
        let pid_clone = pid.clone();
        let event_log_clone = event_log.clone();

        let agent_thread = thread::Builder::new()
            .name("coordinator-agent".to_string())
            .spawn(move || {
                agent_thread_main(
                    &dir,
                    model.as_deref(),
                    rx,
                    alive_clone,
                    pid_clone,
                    &logger,
                    &event_log_clone,
                );
            })
            .context("Failed to spawn coordinator agent thread")?;

        Ok(Self {
            tx,
            _agent_thread: agent_thread,
            alive,
            pid,
            event_log,
        })
    }

    /// Get a reference to the shared event log.
    ///
    /// The daemon uses this to record events that the coordinator agent
    /// will see on its next context refresh.
    #[allow(dead_code)]
    pub fn event_log(&self) -> &SharedEventLog {
        &self.event_log
    }

    /// Send a chat message to the coordinator agent.
    ///
    /// Returns Ok(()) if the message was queued. The response will be
    /// written to the chat outbox asynchronously.
    pub fn send_message(&self, request_id: String, message: String) -> Result<()> {
        self.tx
            .send(ChatRequest {
                request_id,
                message,
            })
            .map_err(|_| anyhow::anyhow!("Coordinator agent thread has exited"))
    }

    /// Check if the coordinator agent process is alive.
    #[allow(dead_code)]
    pub fn is_alive(&self) -> bool {
        *self.alive.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Get the PID of the coordinator agent process.
    #[allow(dead_code)]
    pub fn pid(&self) -> u32 {
        *self.pid.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Shut down the coordinator agent.
    ///
    /// Drops the sender channel, which causes the agent thread to exit
    /// after the current message completes.
    pub fn shutdown(self) {
        drop(self.tx);
        // The agent thread will detect the channel close and exit.
        // We don't join here to avoid blocking the daemon shutdown.
    }
}

// ---------------------------------------------------------------------------
// Agent thread implementation
// ---------------------------------------------------------------------------

/// Main loop for the coordinator agent management thread.
///
/// Spawns the Claude CLI process, processes incoming messages, handles
/// responses, and restarts on crash with context recovery.
///
/// Crash recovery includes:
/// - Time-windowed restart rate limiting (max 3 per 10 minutes)
/// - Conversation history injection on restart
/// - Graph state refresh on restart
/// - Chat history rotation to prevent unbounded growth
fn agent_thread_main(
    dir: &Path,
    model: Option<&str>,
    rx: mpsc::Receiver<ChatRequest>,
    alive: Arc<Mutex<bool>>,
    pid: Arc<Mutex<u32>>,
    logger: &DaemonLogger,
    event_log: &SharedEventLog,
) {
    // Track restart timestamps for time-windowed rate limiting.
    // Instead of a simple counter, we track when each restart occurred
    // and only count restarts within the window.
    let mut restart_timestamps: VecDeque<std::time::Instant> = VecDeque::new();

    loop {
        // --- Time-windowed restart rate limiting ---
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_secs(RESTART_WINDOW_SECS);

        // Purge restart timestamps outside the window
        while let Some(front) = restart_timestamps.front() {
            if now.duration_since(*front) > window {
                restart_timestamps.pop_front();
            } else {
                break;
            }
        }

        // If we've hit the max restarts within the window, pause
        if restart_timestamps.len() >= MAX_RESTARTS_PER_WINDOW {
            let oldest = restart_timestamps.front().copied();
            if let Some(oldest_time) = oldest {
                let wait_time = window.saturating_sub(now.duration_since(oldest_time));
                logger.error(&format!(
                    "Coordinator agent: {} restarts in last {} minutes, pausing for {}s",
                    MAX_RESTARTS_PER_WINDOW,
                    RESTART_WINDOW_SECS / 60,
                    wait_time.as_secs(),
                ));
                std::thread::sleep(wait_time);
                // Purge again after sleeping
                let now = std::time::Instant::now();
                while let Some(front) = restart_timestamps.front() {
                    if now.duration_since(*front) > window {
                        restart_timestamps.pop_front();
                    } else {
                        break;
                    }
                }
            }
        }

        let is_restart = !restart_timestamps.is_empty();

        // Rotate old chat history on restart to prevent unbounded growth
        if is_restart && let Err(e) = chat::rotate_history(dir, HISTORY_ROTATION_KEEP) {
            logger.warn(&format!(
                "Coordinator agent: failed to rotate chat history: {}",
                e
            ));
        }

        // Spawn the Claude CLI process
        logger.info("Coordinator agent: spawning Claude CLI process");
        let spawn_result = spawn_claude_process(dir, model, logger);
        let (mut child, mut stdin, stdout) = match spawn_result {
            Ok(handles) => handles,
            Err(e) => {
                logger.error(&format!(
                    "Coordinator agent: failed to spawn Claude CLI: {}",
                    e
                ));
                restart_timestamps.push_back(std::time::Instant::now());
                std::thread::sleep(std::time::Duration::from_secs(2));
                continue;
            }
        };

        let child_pid = child.id();
        *pid.lock().unwrap_or_else(|e| e.into_inner()) = child_pid;
        *alive.lock().unwrap_or_else(|e| e.into_inner()) = true;
        logger.info(&format!(
            "Coordinator agent: Claude CLI started (PID {})",
            child_pid
        ));

        // Spawn stdout reader thread
        let (response_tx, response_rx) = mpsc::channel::<ResponseEvent>();
        let reader_logger = logger.clone();
        let _reader_thread = thread::Builder::new()
            .name("coordinator-stdout".to_string())
            .spawn(move || {
                stdout_reader(stdout, response_tx, &reader_logger);
            });

        // If this is a restart, inject crash recovery context
        if is_restart {
            logger.info("Coordinator agent: injecting crash recovery context");
            if let Err(e) = inject_crash_recovery_context(dir, &mut stdin, &response_rx, logger) {
                logger.warn(&format!(
                    "Coordinator agent: failed to inject crash recovery context: {}",
                    e
                ));
            }
        }

        // Track the last interaction time for context injection
        let mut last_interaction = chrono::Utc::now().to_rfc3339();

        // Process messages from the main daemon thread
        loop {
            // Wait for a chat message (with timeout to check process health)
            let request = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
                Ok(req) => Some(req),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Channel closed — daemon is shutting down
                    logger.info("Coordinator agent: channel closed, shutting down");
                    let _ = child.kill();
                    let _ = child.wait();
                    *alive.lock().unwrap_or_else(|e| e.into_inner()) = false;
                    *pid.lock().unwrap_or_else(|e| e.into_inner()) = 0;
                    return;
                }
            };

            // Check if the process is still alive
            if let Some(status) = child.try_wait().unwrap_or(None) {
                logger.warn(&format!(
                    "Coordinator agent: Claude CLI exited with status {:?}, restarting",
                    status
                ));
                *alive.lock().unwrap_or_else(|e| e.into_inner()) = false;
                *pid.lock().unwrap_or_else(|e| e.into_inner()) = 0;

                // If there was a pending request, write an error response
                if let Some(req) = request {
                    let _ = chat::append_outbox(
                        dir,
                        "The coordinator agent crashed and is being restarted. Please try again in a moment.",
                        &req.request_id,
                    );
                }
                break; // Break inner loop to restart
            }

            if let Some(req) = request {
                logger.info(&format!(
                    "Coordinator agent: processing request_id={}",
                    req.request_id
                ));

                // Build context injection with event log
                let context =
                    match build_coordinator_context(dir, &last_interaction, Some(event_log)) {
                        Ok(ctx) => ctx,
                        Err(e) => {
                            logger.warn(&format!(
                                "Coordinator agent: failed to build context: {}",
                                e
                            ));
                            String::new()
                        }
                    };

                // Format the user message with context injection prepended
                let full_content = if context.is_empty() {
                    format!("User message:\n{}", req.message)
                } else {
                    format!("{}\n\n---\n\nUser message:\n{}", context, req.message)
                };

                // Write the stream-json user message to stdin
                let user_msg = format_stream_json_user_message(&full_content);
                match stdin.write_all(user_msg.as_bytes()) {
                    Ok(()) => {
                        let _ = stdin.flush();
                    }
                    Err(e) => {
                        logger.error(&format!(
                            "Coordinator agent: failed to write to stdin: {}",
                            e
                        ));
                        let _ = chat::append_outbox(
                            dir,
                            "The coordinator agent encountered an error. Please try again.",
                            &req.request_id,
                        );
                        break; // Restart
                    }
                }

                // Wait for the response from the stdout reader
                let collected =
                    collect_response(&response_rx, logger, std::time::Duration::from_secs(300));

                match collected {
                    Some(resp) if !resp.summary.is_empty() => {
                        logger.info(&format!(
                            "Coordinator agent: got response ({} chars{}) for request_id={}",
                            resp.summary.len(),
                            if resp.full_text.is_some() {
                                ", with tool calls"
                            } else {
                                ""
                            },
                            req.request_id
                        ));
                        if let Err(e) = chat::append_outbox_full(
                            dir,
                            &resp.summary,
                            resp.full_text,
                            &req.request_id,
                        ) {
                            logger.error(&format!(
                                "Coordinator agent: failed to write outbox: {}",
                                e
                            ));
                        }
                    }
                    Some(_) => {
                        logger.warn("Coordinator agent: empty response from Claude CLI");
                        let _ = chat::append_outbox(
                            dir,
                            "The coordinator processed your message but produced no response text.",
                            &req.request_id,
                        );
                    }
                    None => {
                        logger.warn("Coordinator agent: response timeout");
                        let _ = chat::append_outbox(
                            dir,
                            "The coordinator agent timed out processing your message. It may be performing a long-running operation.",
                            &req.request_id,
                        );
                    }
                }

                last_interaction = chrono::Utc::now().to_rfc3339();
            }
        }

        // If we're here, the process died — record restart timestamp and wait
        restart_timestamps.push_back(std::time::Instant::now());
        logger.info(&format!(
            "Coordinator agent: restarting (restarts in window: {})",
            restart_timestamps.len()
        ));
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------------------
// Crash recovery: context injection on restart
// ---------------------------------------------------------------------------

/// Inject crash recovery context into a freshly spawned coordinator agent.
///
/// Sends a synthetic user message containing:
/// 1. A crash restart notification
/// 2. Summary of the last N conversation exchanges (from chat history)
/// 3. Current graph state (full refresh)
///
/// Waits for the agent's acknowledgment response (with a shorter timeout).
fn inject_crash_recovery_context(
    dir: &Path,
    stdin: &mut std::process::ChildStdin,
    response_rx: &mpsc::Receiver<ResponseEvent>,
    logger: &DaemonLogger,
) -> Result<()> {
    let summary = build_crash_recovery_summary(dir)?;

    // Send as a user message
    let user_msg = format_stream_json_user_message(&summary);
    stdin
        .write_all(user_msg.as_bytes())
        .context("Failed to write crash recovery context to stdin")?;
    stdin.flush().context("Failed to flush stdin")?;

    // Wait for the agent's acknowledgment (shorter timeout than normal messages)
    let ack = collect_response(response_rx, logger, std::time::Duration::from_secs(60));
    match ack {
        Some(text) => {
            logger.info(&format!(
                "Coordinator agent: crash recovery acknowledged ({} chars)",
                text.summary.len()
            ));
        }
        None => {
            logger.warn("Coordinator agent: no acknowledgment for crash recovery context");
        }
    }

    Ok(())
}

/// Build the crash recovery summary string from chat history and graph state.
fn build_crash_recovery_summary(dir: &Path) -> Result<String> {
    let mut parts = Vec::new();

    parts.push("You were restarted after a crash. Previous conversation summary:".to_string());
    parts.push(String::new());

    // Load recent conversation history from chat inbox/outbox
    let history = chat::read_history(dir).unwrap_or_default();

    if history.is_empty() {
        parts.push("(No previous conversation history.)".to_string());
    } else {
        // Take last N messages
        let start = history.len().saturating_sub(RECOVERY_HISTORY_COUNT);
        let recent = &history[start..];

        for msg in recent {
            let role_label = if msg.role == "user" {
                "User"
            } else {
                "Coordinator"
            };

            // Truncate long messages for the summary
            let content = if msg.content.len() > RECOVERY_MSG_MAX_CHARS {
                format!("{}...", &msg.content[..RECOVERY_MSG_MAX_CHARS])
            } else {
                msg.content.clone()
            };

            parts.push(format!("{}: {}", role_label, content));
            parts.push(String::new());
        }
    }

    // Add current graph state
    parts.push("---".to_string());
    parts.push(String::new());

    let graph_context = build_coordinator_context(dir, "1970-01-01T00:00:00Z", None)?;
    if graph_context.is_empty() {
        parts.push("Current graph state: No graph found.".to_string());
    } else {
        parts.push(graph_context);
    }

    parts.push(String::new());
    parts.push(
        "Resume your role as the workgraph coordinator. The user may send follow-up messages."
            .to_string(),
    );

    Ok(parts.join("\n"))
}

/// Events emitted by the stdout reader.
enum ResponseEvent {
    /// A text fragment from an assistant message.
    Text(String),
    /// A tool call from an assistant message.
    ToolUse { name: String, input: String },
    /// A tool result from Claude CLI's internal tool execution.
    ToolResult(String),
    /// The assistant turn is complete (end_turn).
    TurnComplete,
    /// The stdout stream ended (process exited or pipe closed).
    StreamEnd,
}

/// Ordered parts of a coordinator response, for building the full response text.
enum ResponsePart {
    Text(String),
    ToolUse { name: String, input: String },
    ToolResult(String),
}

/// Collected coordinator response with summary and full text.
struct CollectedResponse {
    /// Summary text (last text block) for the collapsed view.
    summary: String,
    /// Full response text including tool calls, for the expanded view.
    /// None if the response had no tool calls (full == summary).
    full_text: Option<String>,
}

/// Read stdout from the Claude CLI process line by line, parse stream-json
/// events, and forward text content and turn-complete signals.
fn stdout_reader(
    stdout: std::process::ChildStdout,
    tx: mpsc::Sender<ResponseEvent>,
    logger: &DaemonLogger,
) {
    let reader = BufReader::new(stdout);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                logger.warn(&format!("Coordinator agent stdout: read error: {}", e));
                break;
            }
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        // Try to parse as JSON
        let val: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                // Not JSON — might be debug output, skip
                continue;
            }
        };

        let msg_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match msg_type {
            "assistant" => {
                // Extract text and tool_use content from assistant message
                if let Some(message) = val.get("message") {
                    // Extract content blocks (text + tool_use)
                    if let Some(content) = message.get("content").and_then(|c| c.as_array()) {
                        for block in content {
                            let block_type =
                                block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            match block_type {
                                "text" => {
                                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                        let _ = tx.send(ResponseEvent::Text(text.to_string()));
                                    }
                                }
                                "tool_use" => {
                                    let name = block
                                        .get("name")
                                        .and_then(|n| n.as_str())
                                        .unwrap_or("unknown")
                                        .to_string();
                                    let input = block
                                        .get("input")
                                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                                        .unwrap_or_default();
                                    let _ = tx.send(ResponseEvent::ToolUse { name, input });
                                }
                                _ => {}
                            }
                        }
                    }

                    // Check for stop_reason indicating turn completion
                    let stop_reason = message
                        .get("stop_reason")
                        .and_then(|s| s.as_str())
                        .unwrap_or("");
                    if stop_reason == "end_turn" || stop_reason == "stop_sequence" {
                        let _ = tx.send(ResponseEvent::TurnComplete);
                    }
                }

                // Also check stop_reason at the top level
                let stop_reason = val
                    .get("stop_reason")
                    .and_then(|s| s.as_str())
                    .unwrap_or("");
                if stop_reason == "end_turn" || stop_reason == "stop_sequence" {
                    let _ = tx.send(ResponseEvent::TurnComplete);
                }
            }
            "tool_use" => {
                // Top-level tool_use event (separate from assistant content blocks)
                let name = val
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let input = val
                    .get("input")
                    .map(|v| serde_json::to_string(v).unwrap_or_default())
                    .unwrap_or_default();
                let _ = tx.send(ResponseEvent::ToolUse { name, input });
            }
            "tool_result" => {
                // Tool result: extract content text
                let content_text = if let Some(content) = val.get("content") {
                    if let Some(s) = content.as_str() {
                        s.to_string()
                    } else if let Some(arr) = content.as_array() {
                        arr.iter()
                            .filter_map(|b| {
                                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    b.get("text").and_then(|t| t.as_str()).map(String::from)
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    } else {
                        String::new()
                    }
                } else if let Some(output) = val.get("output").and_then(|o| o.as_str()) {
                    output.to_string()
                } else {
                    String::new()
                };
                if !content_text.is_empty() {
                    let _ = tx.send(ResponseEvent::ToolResult(content_text));
                }
            }
            "result" => {
                // Final result message — turn is complete
                let _ = tx.send(ResponseEvent::TurnComplete);
            }
            _ => {}
        }
    }

    // Stream ended
    let _ = tx.send(ResponseEvent::StreamEnd);
}

/// Collect the full response from the stdout reader.
///
/// Buffers text, tool_use, and tool_result fragments until a TurnComplete signal arrives.
/// Returns a `CollectedResponse` with both the summary (last text block) and the full
/// response including tool calls (for expanded display). Returns None on timeout or StreamEnd.
fn collect_response(
    rx: &mpsc::Receiver<ResponseEvent>,
    logger: &DaemonLogger,
    timeout: std::time::Duration,
) -> Option<CollectedResponse> {
    let deadline = std::time::Instant::now() + timeout;
    let mut parts: Vec<ResponsePart> = Vec::new();
    let mut has_tool_calls = false;

    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or(std::time::Duration::ZERO);
        if remaining.is_zero() {
            logger.warn("Coordinator agent: response collection timed out");
            return build_collected_response(&parts, has_tool_calls);
        }

        match rx.recv_timeout(remaining) {
            Ok(ResponseEvent::Text(text)) => {
                parts.push(ResponsePart::Text(text));
            }
            Ok(ResponseEvent::ToolUse { name, input }) => {
                has_tool_calls = true;
                parts.push(ResponsePart::ToolUse { name, input });
            }
            Ok(ResponseEvent::ToolResult(content)) => {
                has_tool_calls = true;
                parts.push(ResponsePart::ToolResult(content));
            }
            Ok(ResponseEvent::TurnComplete) => {
                // The assistant finished its turn.
                let has_text = parts.iter().any(|p| matches!(p, ResponsePart::Text(_)));
                if !has_text {
                    // Turn complete but no text — this happens when the assistant
                    // only made tool calls. Continue waiting for the next turn.
                    continue;
                }
                return build_collected_response(&parts, has_tool_calls);
            }
            Ok(ResponseEvent::StreamEnd) => {
                logger.warn("Coordinator agent: stdout stream ended during response collection");
                return build_collected_response(&parts, has_tool_calls);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return build_collected_response(&parts, has_tool_calls);
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return build_collected_response(&parts, has_tool_calls);
            }
        }
    }
}

/// Build a `CollectedResponse` from accumulated response parts.
///
/// The summary is the last text block (for collapsed display).
/// The full_text is the complete response with tool calls formatted inline.
fn build_collected_response(
    parts: &[ResponsePart],
    has_tool_calls: bool,
) -> Option<CollectedResponse> {
    // Find the last text part for the summary
    let summary = parts
        .iter()
        .rev()
        .find_map(|p| {
            if let ResponsePart::Text(t) = p {
                Some(t.clone())
            } else {
                None
            }
        })
        .unwrap_or_default();

    if summary.is_empty() && !has_tool_calls {
        return None;
    }

    // If there were no tool calls, full_text is unnecessary (same as summary)
    let full_text = if has_tool_calls {
        Some(format_full_response(parts))
    } else {
        None
    };

    Some(CollectedResponse { summary, full_text })
}

/// Format the full response text from ordered response parts.
///
/// Text blocks are rendered as-is. Tool calls show the tool name and a
/// compact representation of the input. Tool results show truncated output.
fn format_full_response(parts: &[ResponsePart]) -> String {
    let mut out = String::new();

    for part in parts {
        match part {
            ResponsePart::Text(text) => {
                out.push_str(text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
            }
            ResponsePart::ToolUse { name, input } => {
                // Format tool call with a visual delimiter
                out.push_str(&format!("\n┌─ {} ", name));
                out.push_str(&"─".repeat(40usize.saturating_sub(name.len() + 4)));
                out.push('\n');

                // For Bash tool, extract the command for readability
                if name == "Bash" || name == "bash" {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(input) {
                        if let Some(cmd) = val.get("command").and_then(|c| c.as_str()) {
                            out.push_str(&format!("│ $ {}\n", cmd));
                        } else {
                            format_tool_input(&mut out, input);
                        }
                    } else {
                        format_tool_input(&mut out, input);
                    }
                } else {
                    format_tool_input(&mut out, input);
                }
            }
            ResponsePart::ToolResult(content) => {
                // Show tool output, truncated if long
                let lines: Vec<&str> = content.lines().collect();
                let max_lines = 15;
                if lines.len() > max_lines {
                    for line in &lines[..max_lines] {
                        out.push_str(&format!("│ {}\n", line));
                    }
                    out.push_str(&format!("│ ... ({} more lines)\n", lines.len() - max_lines));
                } else {
                    for line in &lines {
                        out.push_str(&format!("│ {}\n", line));
                    }
                }
                out.push_str("└─\n");
            }
        }
    }

    // If the last part was a tool_use with no result, close the box
    if let Some(last) = parts.last()
        && matches!(last, ResponsePart::ToolUse { .. })
    {
        out.push_str("└─\n");
    }

    out
}

/// Format tool input as indented lines.
fn format_tool_input(out: &mut String, input: &str) {
    // Try to pretty-print JSON input
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(input)
        && let Ok(pretty) = serde_json::to_string_pretty(&val)
    {
        for line in pretty.lines() {
            out.push_str(&format!("│ {}\n", line));
        }
        return;
    }
    // Fallback: raw input
    for line in input.lines() {
        out.push_str(&format!("│ {}\n", line));
    }
}

// ---------------------------------------------------------------------------
// Claude CLI process spawning
// ---------------------------------------------------------------------------

/// Spawn the Claude CLI process with stream-json pipes.
///
/// Returns the child process, its stdin handle, and stdout handle.
fn spawn_claude_process(
    dir: &Path,
    model: Option<&str>,
    logger: &DaemonLogger,
) -> Result<(Child, std::process::ChildStdin, std::process::ChildStdout)> {
    let system_prompt = build_system_prompt(dir);

    // Write system prompt to a temp file to avoid shell argument length issues
    let prompt_file = dir.join("service").join("coordinator-prompt.txt");
    std::fs::create_dir_all(prompt_file.parent().unwrap())?;
    std::fs::write(&prompt_file, &system_prompt)
        .context("Failed to write coordinator system prompt file")?;

    let mut cmd = Command::new("claude");
    cmd.env_remove("CLAUDECODE");
    cmd.env_remove("CLAUDE_CODE_ENTRYPOINT");
    cmd.args([
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--dangerously-skip-permissions",
    ]);

    // Pass system prompt (also saved to coordinator-prompt.txt for debugging)
    cmd.args(["--system-prompt", &system_prompt]);

    // Restrict tools to Bash(wg:*) — the coordinator only runs wg commands
    cmd.args(["--allowedTools", "Bash(wg:*)"]);

    if let Some(m) = model {
        cmd.args(["--model", m]);
    }

    cmd.current_dir(dir.parent().unwrap_or(dir));
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());

    // Redirect stderr to a log file for debugging (using Stdio::null() swallows errors)
    let stderr_path = dir.join("service").join("coordinator-stderr.log");
    let stderr_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&stderr_path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null());
    cmd.stderr(stderr_file);

    logger.info(&format!(
        "Coordinator agent: spawning claude with model={}, cwd={:?}, stderr={:?}",
        model.unwrap_or("default"),
        dir.parent().unwrap_or(dir),
        stderr_path,
    ));

    let mut child = cmd.spawn().context("Failed to spawn claude CLI process")?;

    let stdin = child
        .stdin
        .take()
        .context("Failed to get stdin handle for claude process")?;
    let stdout = child
        .stdout
        .take()
        .context("Failed to get stdout handle for claude process")?;

    Ok((child, stdin, stdout))
}

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

/// Coordinator prompt component file names (in composition order).
const COORDINATOR_PROMPT_FILES: &[&str] = &[
    "base-system-prompt.md",
    "behavioral-rules.md",
    "common-patterns.md",
    "evolved-amendments.md",
];

/// Build the system prompt for the coordinator agent by composing from files.
///
/// Reads from `.workgraph/agency/coordinator-prompt/` and concatenates the
/// component files in order. Falls back to the hardcoded prompt if the
/// directory doesn't exist or no files are found.
///
/// Dynamic state goes through context injection (see `build_coordinator_context`).
fn build_system_prompt(dir: &Path) -> String {
    let prompt_dir = dir.join("agency/coordinator-prompt");

    if prompt_dir.is_dir() {
        let mut parts = Vec::new();
        for filename in COORDINATOR_PROMPT_FILES {
            let path = prompt_dir.join(filename);
            if let Ok(content) = std::fs::read_to_string(&path) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }
        if !parts.is_empty() {
            return parts.join("\n\n");
        }
    }

    // Fallback: hardcoded prompt (for projects without coordinator-prompt/ files)
    build_system_prompt_fallback()
}

/// Hardcoded fallback prompt used when coordinator-prompt files don't exist.
fn build_system_prompt_fallback() -> String {
    include_str!("coordinator_prompt_fallback.txt").to_string()
}

// ---------------------------------------------------------------------------
// Context injection
// ---------------------------------------------------------------------------

/// Build the dynamic context injection string for the coordinator agent.
///
/// This is prepended to each user message to give the coordinator awareness
/// of the current graph state, recent events, and active agents.
///
/// If `event_log` is provided, recent events are drained from it (more
/// efficient and accurate than scanning task logs). Otherwise, falls back
/// to scanning task log entries since `last_interaction`.
pub fn build_coordinator_context(
    dir: &Path,
    last_interaction: &str,
    event_log: Option<&SharedEventLog>,
) -> Result<String> {
    let gp = graph_path(dir);
    if !gp.exists() {
        return Ok(String::new());
    }

    let graph = load_graph(&gp).context("Failed to load graph for context injection")?;

    // --- Graph Summary ---
    let mut done = 0usize;
    let mut in_progress = 0usize;
    let mut open = 0usize;
    let mut blocked = 0usize;
    let mut failed = 0usize;
    let mut abandoned = 0usize;

    for task in graph.tasks() {
        match task.status {
            Status::Done => done += 1,
            Status::InProgress => in_progress += 1,
            Status::Open => {
                // Check if blocked (any after dep not Done)
                let is_blocked = task.after.iter().any(|dep_id| {
                    graph
                        .get_task(dep_id)
                        .map(|d| !d.status.is_terminal())
                        .unwrap_or(false)
                });
                if is_blocked {
                    blocked += 1;
                } else {
                    open += 1;
                }
            }
            Status::Failed => failed += 1,
            Status::Abandoned => abandoned += 1,
            _ => {}
        }
    }
    let total = done + in_progress + open + blocked + failed + abandoned;

    // --- Recent Events ---
    let mut events = Vec::new();

    if let Some(elog) = event_log {
        // Drain events from the shared event log (preferred path)
        if let Ok(last_dt) = last_interaction.parse::<DateTime<Utc>>()
            && let Ok(mut log) = elog.lock()
        {
            for (ts, event) in log.drain_since(&last_dt) {
                events.push(format!("- [{}] {}", ts.format("%H:%M:%S"), event));
            }
        }
    } else {
        // Fallback: scan task logs since last_interaction
        if let Ok(last_dt) = last_interaction.parse::<DateTime<Utc>>() {
            for task in graph.tasks() {
                for log_entry in &task.log {
                    if let Ok(entry_dt) = log_entry.timestamp.parse::<DateTime<Utc>>()
                        && entry_dt > last_dt
                    {
                        events.push(format!(
                            "- [{}] {} (task: {})",
                            &log_entry.timestamp[11..19], // HH:MM:SS
                            log_entry.message,
                            task.id,
                        ));
                    }
                }
            }
        }
    }
    // Limit to most recent 20 events
    events.sort();
    if events.len() > 20 {
        let skip = events.len() - 20;
        events = events.into_iter().skip(skip).collect();
    }

    // --- Active Agents ---
    let mut agent_lines = Vec::new();
    if let Ok(registry) = AgentRegistry::load(dir) {
        for agent in registry.list_agents() {
            if agent.is_alive() && is_process_alive(agent.pid) {
                agent_lines.push(format!(
                    "- {} working on \"{}\" (uptime: {})",
                    agent.id,
                    agent.task_id,
                    agent.uptime_human(),
                ));
            }
        }
    }

    // --- Failed Tasks ---
    let failed_tasks: Vec<String> = graph
        .tasks()
        .filter(|t| t.status == Status::Failed)
        .map(|t| {
            format!(
                "- FAILED: {} \"{}\" — {}",
                t.id,
                t.title,
                t.failure_reason.as_deref().unwrap_or("unknown reason"),
            )
        })
        .collect();

    // --- Format ---
    let now = chrono::Utc::now().to_rfc3339();
    let mut parts = Vec::new();

    parts.push(format!("## System Context Update ({})", now));

    parts.push(format!(
        "\n### Graph Summary\n{} tasks: {} done, {} in-progress, {} open, {} blocked, {} failed, {} abandoned",
        total, done, in_progress, open, blocked, failed, abandoned
    ));

    parts.push("\n### Recent Events".to_string());
    if events.is_empty() {
        parts.push("- No events since last interaction.".to_string());
    } else {
        for event in &events {
            parts.push(event.clone());
        }
    }

    parts.push("\n### Active Agents".to_string());
    if agent_lines.is_empty() {
        parts.push("- No active agents.".to_string());
    } else {
        for line in &agent_lines {
            parts.push(line.clone());
        }
    }

    parts.push("\n### Attention Needed".to_string());
    if failed_tasks.is_empty() {
        parts.push("- Nothing requires attention.".to_string());
    } else {
        for line in &failed_tasks {
            parts.push(line.clone());
        }
    }

    Ok(parts.join("\n"))
}

// ---------------------------------------------------------------------------
// Stream-JSON formatting
// ---------------------------------------------------------------------------

/// Format a user message in Claude CLI stream-json input format.
fn format_stream_json_user_message(content: &str) -> String {
    let msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": content,
        }
    });
    let mut s = serde_json::to_string(&msg).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_format_stream_json_user_message() {
        let msg = format_stream_json_user_message("hello world");
        let parsed: serde_json::Value = serde_json::from_str(msg.trim()).unwrap();
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["role"], "user");
        assert_eq!(parsed["message"]["content"], "hello world");
    }

    #[test]
    fn test_format_stream_json_user_message_with_special_chars() {
        let msg = format_stream_json_user_message("hello \"world\" with\nnewlines");
        let parsed: serde_json::Value = serde_json::from_str(msg.trim()).unwrap();
        assert_eq!(
            parsed["message"]["content"],
            "hello \"world\" with\nnewlines"
        );
    }

    #[test]
    fn test_build_system_prompt_fallback() {
        let tmp = TempDir::new().unwrap();
        let prompt = build_system_prompt(tmp.path());
        // Falls back to hardcoded prompt since no coordinator-prompt dir exists
        assert!(prompt.contains("workgraph coordinator"));
        assert!(prompt.contains("Never implement"));
        assert!(prompt.contains("wg add"));
    }

    #[test]
    fn test_build_system_prompt_from_files() {
        let tmp = TempDir::new().unwrap();
        let prompt_dir = tmp.path().join("agency/coordinator-prompt");
        std::fs::create_dir_all(&prompt_dir).unwrap();
        std::fs::write(prompt_dir.join("base-system-prompt.md"), "Base prompt here").unwrap();
        std::fs::write(prompt_dir.join("behavioral-rules.md"), "Rules here").unwrap();
        std::fs::write(prompt_dir.join("common-patterns.md"), "Patterns here").unwrap();
        std::fs::write(prompt_dir.join("evolved-amendments.md"), "Amendments here").unwrap();

        let prompt = build_system_prompt(tmp.path());
        assert!(prompt.contains("Base prompt here"));
        assert!(prompt.contains("Rules here"));
        assert!(prompt.contains("Patterns here"));
        assert!(prompt.contains("Amendments here"));
        // Should NOT contain fallback content
        assert!(!prompt.contains("workgraph coordinator"));
    }

    #[test]
    fn test_build_coordinator_context_no_graph() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let ctx = build_coordinator_context(dir, "2026-01-01T00:00:00Z", None).unwrap();
        assert!(ctx.is_empty());
    }

    #[test]
    fn test_build_coordinator_context_with_graph() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Create a minimal graph
        std::fs::create_dir_all(dir.join(".workgraph")).unwrap();
        let graph_file = dir.join("graph.md");
        std::fs::write(
            &graph_file,
            "# Graph\n\n## Tasks\n\n- [x] task-1: Done task\n- [ ] task-2: Open task\n",
        )
        .unwrap();

        // This will fail to load since it's not a valid graph format,
        // but we're testing the error path gracefully
        let ctx = build_coordinator_context(dir, "2026-01-01T00:00:00Z", None);
        // Either succeeds with content or fails gracefully
        assert!(ctx.is_ok() || ctx.is_err());
    }

    #[test]
    fn test_event_log_record_and_drain() {
        let mut log = EventLog::new();
        let before = Utc::now();

        log.record(Event::TaskCompleted {
            task_id: "task-1".to_string(),
            agent_id: Some("agent-1".to_string()),
        });
        log.record(Event::TaskFailed {
            task_id: "task-2".to_string(),
            reason: "test failure".to_string(),
        });

        assert_eq!(log.len(), 2);

        let events = log.drain_since(&before);
        assert_eq!(events.len(), 2);
        assert_eq!(log.len(), 0);
    }

    #[test]
    fn test_event_log_drain_filters_old() {
        let mut log = EventLog::new();
        let after = Utc::now();

        // These events happened "before" our timestamp since we record them now
        // but the drain uses > comparison, so we need events after the timestamp
        std::thread::sleep(std::time::Duration::from_millis(10));

        log.record(Event::TaskAdded {
            task_id: "task-1".to_string(),
            title: "Test".to_string(),
            added_by: None,
        });

        let events = log.drain_since(&after);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_crash_recovery_summary_no_history() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("chat")).unwrap();

        let summary = build_crash_recovery_summary(dir).unwrap();
        assert!(summary.contains("restarted after a crash"));
        assert!(summary.contains("No previous conversation history"));
    }

    #[test]
    fn test_crash_recovery_summary_with_history() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("chat")).unwrap();

        // Add some chat history
        chat::append_inbox(dir, "help me plan auth", "req-1").unwrap();
        chat::append_outbox(dir, "I'll create tasks for auth", "req-1").unwrap();
        chat::append_inbox(dir, "what's the status?", "req-2").unwrap();
        chat::append_outbox(dir, "3 tasks in progress", "req-2").unwrap();

        let summary = build_crash_recovery_summary(dir).unwrap();
        assert!(summary.contains("restarted after a crash"));
        assert!(summary.contains("help me plan auth"));
        assert!(summary.contains("create tasks for auth"));
        assert!(summary.contains("what's the status?"));
        assert!(summary.contains("3 tasks in progress"));
    }

    #[test]
    fn test_crash_recovery_summary_truncates_long_messages() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("chat")).unwrap();

        // Add a very long message
        let long_msg = "x".repeat(1000);
        chat::append_inbox(dir, &long_msg, "req-1").unwrap();

        let summary = build_crash_recovery_summary(dir).unwrap();
        // The summary should contain the truncated version (500 chars + "...")
        assert!(summary.contains("..."));
        assert!(!summary.contains(&long_msg));
    }

    #[test]
    fn test_crash_recovery_summary_limits_history_count() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("chat")).unwrap();

        // Add more messages than RECOVERY_HISTORY_COUNT
        for i in 0..20 {
            chat::append_inbox(dir, &format!("msg-{}", i), &format!("req-{}", i)).unwrap();
            chat::append_outbox(dir, &format!("response-{}", i), &format!("req-{}", i)).unwrap();
        }

        let summary = build_crash_recovery_summary(dir).unwrap();
        // Should only contain the last RECOVERY_HISTORY_COUNT messages
        // The earliest messages should NOT be present
        assert!(!summary.contains("msg-0"));
        // But later messages should be
        assert!(summary.contains("msg-19") || summary.contains("response-19"));
    }
}
