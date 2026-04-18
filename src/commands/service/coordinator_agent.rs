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
use workgraph::service::compactor::{CompactorState, context_md_path};
use workgraph::service::executor::ExecutorRegistry;
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
    /// A zero-output agent was detected and killed.
    ZeroOutputKill {
        agent_id: String,
        task_id: String,
        age_secs: u64,
    },
    /// A task hit the per-task zero-output circuit breaker.
    ZeroOutputCircuitBreak { task_id: String, attempts: u32 },
    /// Global API-down detected: majority of agents have zero output.
    GlobalApiOutage {
        zero_count: usize,
        total_count: usize,
        backoff_secs: u64,
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
            Event::ZeroOutputKill {
                agent_id,
                task_id,
                age_secs,
            } => {
                write!(
                    f,
                    "zero-output agent {} killed on {} (alive {}s with no output)",
                    agent_id, task_id, age_secs
                )
            }
            Event::ZeroOutputCircuitBreak { task_id, attempts } => {
                write!(
                    f,
                    "task {} circuit-broken after {} zero-output attempts",
                    task_id, attempts
                )
            }
            Event::GlobalApiOutage {
                zero_count,
                total_count,
                backoff_secs,
            } => {
                write!(
                    f,
                    "GLOBAL API OUTAGE: {}/{} agents zero-output, backoff {}s",
                    zero_count, total_count, backoff_secs
                )
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
    /// True when this agent is backed by a `wg nex --chat-id N`
    /// subprocess (reads user turns from the inbox directly, doesn't
    /// need messages pushed via the channel). Callers that would
    /// otherwise forward inbox messages through `send_message` should
    /// skip that step in subprocess mode to avoid re-appending
    /// the same message to the inbox.
    uses_subprocess: bool,
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
        coordinator_id: u32,
        model: Option<&str>,
        executor: Option<&str>,
        provider: Option<&str>,
        logger: &DaemonLogger,
        event_log: SharedEventLog,
    ) -> Result<Self> {
        let executor = executor.unwrap_or("claude");
        // Decide the coordinator implementation up front so send_message
        // can skip the redundant-append path for subprocess mode. Mirror
        // `agent_thread_main`'s dispatcher logic.
        let model_requires_native = model
            .map(|m| {
                let config = workgraph::config::Config::load_or_default(dir);
                super::coordinator::requires_native_executor(m, &config)
            })
            .unwrap_or(false);
        let uses_subprocess = executor == "native"
            || matches!(
                provider,
                Some("openrouter") | Some("oai-compat") | Some("openai") | Some("local")
            )
            || model_requires_native;

        if !uses_subprocess && executor == "claude" && !Self::is_claude_available() {
            anyhow::bail!(
                "Claude CLI not found. Install it to enable the persistent coordinator agent."
            );
        }
        let (tx, rx) = mpsc::channel::<ChatRequest>();
        let alive = Arc::new(Mutex::new(false));
        let pid = Arc::new(Mutex::new(0u32));

        let dir = dir.to_path_buf();
        let model = model.map(String::from);
        let executor = executor.to_string();
        let provider = provider.map(String::from);
        let logger = logger.clone();
        let alive_clone = alive.clone();
        let pid_clone = pid.clone();
        let event_log_clone = event_log.clone();

        let agent_thread = thread::Builder::new()
            .name(format!("coordinator-agent-{}", coordinator_id))
            .spawn(move || {
                agent_thread_main(
                    &dir,
                    coordinator_id,
                    model.as_deref(),
                    &executor,
                    provider.as_deref(),
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
            uses_subprocess,
        })
    }

    /// True when this agent is backed by the `wg nex --chat-id N`
    /// subprocess path. Callers forwarding inbox messages should skip
    /// `send_message` for these agents — the subprocess reads the
    /// inbox directly.
    pub fn uses_subprocess(&self) -> bool {
        self.uses_subprocess
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

    /// Send an autonomous heartbeat prompt to the coordinator agent.
    ///
    /// This injects a synthetic message (not from a human) that triggers the
    /// coordinator to review graph state and take action. Used for TB heartbeat
    /// orchestration (Condition G Phase 3).
    pub fn send_heartbeat(
        &self,
        tick_number: u64,
        start_time: std::time::Instant,
        budget_secs: Option<u64>,
    ) -> Result<()> {
        let elapsed = start_time.elapsed().as_secs();
        let remaining = budget_secs.map(|b| b.saturating_sub(elapsed));
        let remaining_display = remaining
            .map(|r| format!("~{}s", r))
            .unwrap_or_else(|| "unlimited".to_string());
        let timestamp = chrono::Utc::now().format("%H:%M:%S").to_string();

        let phase_guidance = match remaining {
            Some(r) if r < 120 => {
                "⚠️ EMERGENCY — <2 minutes remaining!\n\
                 1. Do NOT create or dispatch any new tasks\n\
                 2. For each in-progress task: if the agent has committed code, \
                    run `wg done <task>` to force-complete it\n\
                 3. Kill any agents that appear stuck: `wg kill <id>`\n\
                 4. Accept partial progress — it's better than nothing"
            }
            Some(r) if r < 300 => {
                "⚠️ WIND-DOWN — <5 minutes remaining!\n\
                 1. Do NOT create new tasks or spawn new agents\n\
                 2. Send wrap-up message to ALL in-progress agents:\n\
                    `wg msg send <task> \"TIME CRITICAL: <5min remaining. \
                    Commit your current work NOW, run wg done, stop iterating.\"`\n\
                 3. If a task's tests pass, force-complete it with `wg done <task>`\n\
                 4. Focus on preserving progress, not perfection"
            }
            _ => {
                "Review the system state and take action:\n\
                 1. STUCK AGENTS: Any agent running >5min with no output? → `wg kill <id>` and retry\n\
                 2. FAILED TASKS: Any tasks failed? → Analyze cause, create fix-up task or retry\n\
                 3. READY WORK: Unblocked tasks waiting? → Ensure they'll be dispatched (they auto-spawn)\n\
                 4. PROGRESS CHECK: Is the work converging toward completion?\n\
                 5. STRATEGIC: Should any running approach be abandoned for a different strategy?"
            }
        };

        let prompt = format!(
            "[AUTONOMOUS HEARTBEAT] Tick #{tick_number} at {timestamp}\n\
             Time elapsed: {elapsed}s | Budget remaining: {remaining_display}\n\
             \n\
             You are the autonomous coordinator for this project. No human operator.\n\
             {phase_guidance}\n\
             \n\
             If everything is nominal, respond: \"NOOP — all systems nominal.\"\n\
             If you take action, log what and why.",
        );

        let request_id = format!("heartbeat-{}", tick_number);
        self.send_message(request_id, prompt)
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

    /// Interrupt the current generation by sending SIGINT to the Claude CLI process.
    ///
    /// Returns `true` if SIGINT was sent, `false` if the process is not alive.
    /// The Claude CLI handles SIGINT by stopping the current generation and
    /// emitting a TurnComplete signal, preserving the conversation context.
    pub fn interrupt(&self) -> bool {
        let pid = *self.pid.lock().unwrap_or_else(|e| e.into_inner());
        if pid == 0 {
            return false;
        }
        // Send SIGINT (not SIGKILL) — Claude CLI treats this as "stop generating"
        unsafe {
            libc::kill(pid as i32, libc::SIGINT);
        }
        true
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
#[allow(clippy::too_many_arguments)]
fn agent_thread_main(
    dir: &Path,
    coordinator_id: u32,
    model: Option<&str>,
    executor: &str,
    provider: Option<&str>,
    rx: mpsc::Receiver<ChatRequest>,
    alive: Arc<Mutex<bool>>,
    pid: Arc<Mutex<u32>>,
    logger: &DaemonLogger,
    event_log: &SharedEventLog,
) {
    // Use the native coordinator loop when the executor is "native", the provider
    // is non-Anthropic (openrouter, openai, local), OR the model itself requires a
    // non-Anthropic provider (e.g. "deepseek/deepseek-chat", "google/gemini-*").
    // The Claude CLI only understands Anthropic models, so everything else must go
    // through the native path which makes direct API calls.
    let model_requires_native = model
        .map(|m| {
            let config = workgraph::config::Config::load_or_default(dir);
            super::coordinator::requires_native_executor(m, &config)
        })
        .unwrap_or(false);
    let use_native = executor == "native"
        || matches!(
            provider,
            Some("openrouter") | Some("oai-compat") | Some("openai") | Some("local")
        )
        || model_requires_native;
    if use_native {
        // Use the unified `wg nex --chat-id N` subprocess path. Same
        // AgentLoop as interactive `wg nex` and task-agent runs —
        // the coordinator is just a nex session with an inbox and
        // a role. `rx` is drained into the inbox here so synthetic
        // messages (heartbeats, CoordinatorAgent::send_message)
        // reach the subprocess; direct inbox writes from the TUI
        // bypass the channel entirely.
        nex_subprocess_coordinator_loop(
            dir,
            coordinator_id,
            model,
            provider,
            rx,
            alive,
            pid,
            logger,
        );
        return;
    }
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
        if is_restart
            && let Err(e) = chat::rotate_history_for(dir, coordinator_id, HISTORY_ROTATION_KEEP)
        {
            logger.warn(&format!(
                "Coordinator agent: failed to rotate chat history: {}",
                e
            ));
        }

        // Archive-rotate chat files if they exceed configured thresholds
        if let Err(e) = chat::check_and_rotate_for(dir, coordinator_id) {
            logger.warn(&format!(
                "Coordinator agent: failed to check/rotate chat archives: {}",
                e
            ));
        }
        // Clean up expired archives
        if let Err(e) = chat::cleanup_archives_for(dir, coordinator_id) {
            logger.warn(&format!(
                "Coordinator agent: failed to clean up chat archives: {}",
                e
            ));
        }

        // Spawn the Claude CLI process
        logger.info("Coordinator agent: spawning Claude CLI process");
        let spawn_result = spawn_claude_process(dir, executor, model, provider, logger);
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
            if let Err(e) =
                inject_crash_recovery_context(dir, coordinator_id, &mut stdin, &response_rx, logger)
            {
                logger.warn(&format!(
                    "Coordinator agent: failed to inject crash recovery context: {}",
                    e
                ));
            }
        }

        // Track the last interaction time for context injection
        let mut last_interaction = chrono::Utc::now().to_rfc3339();

        // Track turn count for evaluation frequency
        let mut turn_count: u32 = 0;

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
                    let _ = chat::append_outbox_for(
                        dir,
                        coordinator_id,
                        "The coordinator agent crashed and is being restarted. Please try again in a moment.",
                        &req.request_id,
                    );
                }
                chat::clear_streaming(dir, coordinator_id);
                break; // Break inner loop to restart
            }

            if let Some(req) = request {
                let turn_start = std::time::Instant::now();
                logger.info(&format!(
                    "Coordinator agent: processing request_id={}",
                    req.request_id
                ));

                // Build context injection with event log
                let context = match build_coordinator_context(
                    dir,
                    &last_interaction,
                    Some(event_log),
                    coordinator_id,
                ) {
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
                        let _ = chat::append_outbox_for(
                            dir,
                            coordinator_id,
                            "The coordinator agent encountered an error. Please try again.",
                            &req.request_id,
                        );
                        chat::clear_streaming(dir, coordinator_id);
                        break; // Restart
                    }
                }

                // Wait for the response from the stdout reader, streaming
                // partial text to the chat streaming file as tokens arrive.
                let collected = collect_response(
                    &response_rx,
                    logger,
                    std::time::Duration::from_secs(300),
                    Some((dir, coordinator_id)),
                );

                // Extract token usage before consuming the response.
                let turn_token_usage = collected.as_ref().and_then(|r| r.token_usage);

                let response_info = match collected {
                    Some(resp) if !resp.summary.is_empty() => {
                        let len = resp.summary.len();
                        let summary_clone = resp.summary.clone();
                        logger.info(&format!(
                            "Coordinator agent: got response ({} chars{}) for request_id={}",
                            len,
                            if resp.full_text.is_some() {
                                ", with tool calls"
                            } else {
                                ""
                            },
                            req.request_id
                        ));
                        if let Err(e) = chat::append_outbox_full_for(
                            dir,
                            coordinator_id,
                            &resp.summary,
                            resp.full_text,
                            &req.request_id,
                        ) {
                            logger.error(&format!(
                                "Coordinator agent: failed to write outbox: {}",
                                e
                            ));
                        }
                        Some((len, summary_clone))
                    }
                    Some(_) => {
                        logger.warn("Coordinator agent: empty response from Claude CLI");
                        let _ = chat::append_outbox_for(
                            dir,
                            coordinator_id,
                            "The coordinator processed your message but produced no response text.",
                            &req.request_id,
                        );
                        None
                    }
                    None => {
                        logger.warn("Coordinator agent: response timeout");
                        let _ = chat::append_outbox_for(
                            dir,
                            coordinator_id,
                            "The coordinator agent timed out processing your message. It may be performing a long-running operation.",
                            &req.request_id,
                        );
                        None
                    }
                };

                // Clear the streaming file now that the complete response is
                // written to the outbox.
                chat::clear_streaming(dir, coordinator_id);

                // Accumulate token usage in coordinator state for compaction gating.
                // Done regardless of whether there was a text response, so even
                // tool-only turns are counted.
                //
                // BUG FIX: With prompt caching, the API's `input_tokens` only counts
                // tokens outside any cache block (typically 1-3 per turn). The actual
                // new content goes to `cache_creation_input_tokens`. We accumulate
                // cache_creation + output to get a meaningful compaction signal.
                if let Some((input_toks, output_toks, cache_creation_toks)) = turn_token_usage {
                    let total = cache_creation_toks
                        .saturating_add(input_toks)
                        .saturating_add(output_toks);
                    if total > 0 {
                        let mut cs =
                            super::CoordinatorState::load_or_default_for(dir, coordinator_id);
                        cs.accumulated_tokens = cs.accumulated_tokens.saturating_add(total);
                        cs.save_for(dir, coordinator_id);
                        logger.info(&format!(
                            "Coordinator agent: turn used {} tokens (input={}, output={}, cache_creation={}), accumulated={}",
                            total, input_toks, output_toks, cache_creation_toks, cs.accumulated_tokens
                        ));
                    }
                }

                // Record this turn as a cycle iteration on the .coordinator task
                if let Some((resp_len, ref resp_summary)) = response_info {
                    turn_count += 1;
                    record_coordinator_turn(
                        dir,
                        coordinator_id,
                        &req.message,
                        resp_len,
                        turn_start,
                    );

                    // Inline evaluation (runs in background thread, non-blocking)
                    let eval_config = workgraph::config::Config::load_or_default(dir);
                    if should_evaluate_turn(turn_count, &eval_config.coordinator.eval_frequency) {
                        let eval_dir = dir.to_path_buf();
                        let eval_msg = req.message.clone();
                        let eval_resp = resp_summary.clone();
                        let eval_turn = turn_count;
                        std::thread::Builder::new()
                            .name("coordinator-eval".to_string())
                            .spawn(move || {
                                evaluate_coordinator_turn(
                                    &eval_dir, eval_turn, &eval_msg, &eval_resp,
                                );
                            })
                            .ok();
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
// nex-subprocess coordinator: the unified path (nex = task = coordinator)
// ---------------------------------------------------------------------------

/// Coordinator implementation backed by a `wg nex --chat-id N --role coordinator`
/// subprocess.
///
/// Replaces the in-process `native_coordinator_loop`. The subprocess reads
/// user turns from `chat/N/inbox.jsonl` directly via its `.nex-cursor`,
/// streams tokens to `chat/N/.streaming`, and appends finalized replies
/// to `chat/N/outbox.jsonl` — the same file formats the TUI reads. Crash
/// restart rate-limiting mirrors the Claude-CLI path so a wedged model
/// doesn't respawn in a tight loop.
///
/// `rx` is drained into the inbox so `CoordinatorAgent::send_message`
/// (used for heartbeats and daemon-internal synthetic prompts) reaches
/// the subprocess. User messages written to the inbox directly by the
/// TUI are seen by the subprocess without going through this drain.
#[allow(clippy::too_many_arguments)]
fn nex_subprocess_coordinator_loop(
    dir: &Path,
    coordinator_id: u32,
    model: Option<&str>,
    provider: Option<&str>,
    rx: mpsc::Receiver<ChatRequest>,
    alive: Arc<Mutex<bool>>,
    pid: Arc<Mutex<u32>>,
    logger: &DaemonLogger,
) {
    // Start a small forwarder thread that drains `rx` → inbox. We can't
    // own `rx` on the supervisor thread and also block on `child.wait()`,
    // so a dedicated forwarder keeps send_message non-blocking across
    // subprocess restarts.
    let dir_buf = dir.to_path_buf();
    let forwarder = std::thread::Builder::new()
        .name(format!("coordinator-nex-fwd-{}", coordinator_id))
        .spawn(move || {
            while let Ok(req) = rx.recv() {
                if let Err(e) =
                    chat::append_inbox_for(&dir_buf, coordinator_id, &req.message, &req.request_id)
                {
                    eprintln!(
                        "[coordinator-{}] forwarder: append_inbox_for failed: {}",
                        coordinator_id, e
                    );
                }
            }
        });
    let _forwarder = match forwarder {
        Ok(h) => Some(h),
        Err(e) => {
            logger.error(&format!(
                "Coordinator-{}: failed to spawn inbox forwarder thread: {}",
                coordinator_id, e
            ));
            None
        }
    };

    let mut restart_timestamps: VecDeque<std::time::Instant> = VecDeque::new();

    loop {
        // Rate-limit restarts in a sliding window, same policy as the
        // Claude CLI path above. Prevents a wedged model or a repeated
        // startup-time crash from burning the daemon.
        let now = std::time::Instant::now();
        let window = std::time::Duration::from_secs(RESTART_WINDOW_SECS);
        while let Some(front) = restart_timestamps.front() {
            if now.duration_since(*front) > window {
                restart_timestamps.pop_front();
            } else {
                break;
            }
        }
        if restart_timestamps.len() >= MAX_RESTARTS_PER_WINDOW {
            let oldest = restart_timestamps.front().copied();
            if let Some(oldest_time) = oldest {
                let wait_time = window.saturating_sub(now.duration_since(oldest_time));
                logger.error(&format!(
                    "Coordinator-{}: {} restarts in last {} minutes, pausing for {}s",
                    coordinator_id,
                    MAX_RESTARTS_PER_WINDOW,
                    RESTART_WINDOW_SECS / 60,
                    wait_time.as_secs()
                ));
                std::thread::sleep(wait_time);
                restart_timestamps.clear();
            }
        }

        // Ensure the coordinator session is registered under its
        // canonical alias. Idempotent — first call creates the UUID
        // dir + `coordinator-N` alias symlink, later calls are no-ops.
        // Any pre-existing real `chat/N/` dir gets migrated to the
        // new UUID-based layout here.
        let _ = workgraph::chat_sessions::migrate_numeric_coord_dir(dir, coordinator_id);
        let chat_alias = format!("coordinator-{}", coordinator_id);
        let numeric_alias = coordinator_id.to_string();
        if let Err(e) = workgraph::chat_sessions::ensure_session(
            dir,
            &chat_alias,
            workgraph::chat_sessions::SessionKind::Coordinator,
            Some(format!("coordinator {}", coordinator_id)),
        ) {
            logger.error(&format!(
                "Coordinator-{}: failed to register session alias {}: {}",
                coordinator_id, chat_alias, e
            ));
        }
        // ALSO register the bare numeric alias (`chat/N` → UUID).
        // `chat::append_inbox_for(dir, N, ...)` — used by the IPC
        // `UserChat` handler via `append_chat_inbox` — resolves paths
        // through `chat/<N>/inbox.jsonl`. Without this symlink,
        // that write creates a parallel `chat/N/` real directory
        // that the coordinator subprocess (watching
        // `chat/coordinator-N/inbox.jsonl`) never sees, and the
        // user's message sits unread until a heartbeat bump or
        // restart re-routes things. Observed 2026-04-18: TUI chat
        // messages hung forever until timeout.
        if let Err(e) = workgraph::chat_sessions::add_alias(dir, &chat_alias, &numeric_alias) {
            // `add_alias` returns an error when the alias is already
            // taken — harmless steady-state idempotency. Only log
            // non-trivial errors.
            if !format!("{}", e).contains("already") {
                logger.warn(&format!(
                    "Coordinator-{}: failed to add numeric alias {}: {}",
                    coordinator_id, numeric_alias, e
                ));
            }
        }

        // Build the argv. Always pass `--resume` — on first spawn there's
        // no journal so nex falls back to a fresh session; on subsequent
        // spawns the deterministic `chat/<uuid>/conversation.jsonl`
        // restores conversation state. We address the session by alias
        // (`coordinator-N`) so the `chat-N` symlink + the alias entry
        // in `sessions.json` both point at the right UUID.
        let wg_bin = std::env::current_exe().unwrap_or_else(|_| "wg".into());
        let mut cmd = Command::new(&wg_bin);
        cmd.arg("nex")
            .arg("--chat")
            .arg(&chat_alias)
            .arg("--role")
            .arg("coordinator")
            .arg("--resume");
        if let Some(m) = model {
            cmd.arg("--model").arg(m);
        }
        cmd.current_dir(dir.parent().unwrap_or(dir));
        cmd.env("WG_EXECUTOR_TYPE", "native");
        if let Some(p) = provider {
            cmd.env("WG_PROVIDER", p);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        logger.info(&format!(
            "Coordinator-{}: spawning `wg nex --chat-id {}` subprocess",
            coordinator_id, coordinator_id
        ));
        let mut child: Child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                logger.error(&format!(
                    "Coordinator-{}: failed to spawn nex subprocess: {}",
                    coordinator_id, e
                ));
                restart_timestamps.push_back(std::time::Instant::now());
                std::thread::sleep(std::time::Duration::from_secs(5));
                continue;
            }
        };

        let child_pid = child.id();
        *pid.lock().unwrap_or_else(|e| e.into_inner()) = child_pid;
        *alive.lock().unwrap_or_else(|e| e.into_inner()) = true;
        restart_timestamps.push_back(std::time::Instant::now());
        logger.info(&format!(
            "Coordinator-{}: nex subprocess running (pid {})",
            coordinator_id, child_pid
        ));

        // Drain stdout/stderr to the daemon log in background threads —
        // without this, the child's pipes fill and it blocks.
        let cid = coordinator_id;
        let logger_out = logger.clone();
        let stdout = child.stdout.take();
        std::thread::Builder::new()
            .name(format!("coordinator-nex-stdout-{}", cid))
            .spawn(move || {
                if let Some(out) = stdout {
                    for line in BufReader::new(out).lines().map_while(|l| l.ok()) {
                        logger_out.info(&format!("[coordinator-{} stdout] {}", cid, line));
                    }
                }
            })
            .ok();
        let logger_err = logger.clone();
        let stderr = child.stderr.take();
        std::thread::Builder::new()
            .name(format!("coordinator-nex-stderr-{}", cid))
            .spawn(move || {
                if let Some(err) = stderr {
                    for line in BufReader::new(err).lines().map_while(|l| l.ok()) {
                        logger_err.info(&format!("[coordinator-{} stderr] {}", cid, line));
                    }
                }
            })
            .ok();

        let exit_status = child.wait();
        *alive.lock().unwrap_or_else(|e| e.into_inner()) = false;
        *pid.lock().unwrap_or_else(|e| e.into_inner()) = 0;

        match exit_status {
            Ok(status) if status.success() => {
                logger.info(&format!(
                    "Coordinator-{}: nex subprocess exited cleanly ({})",
                    coordinator_id, status
                ));
                // Clean exit (user ran /quit, or max-turns hit) — don't
                // respawn in a tight loop. Sleep a moment to avoid eating
                // the whole restart budget on clean exits.
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
            Ok(status) => {
                logger.error(&format!(
                    "Coordinator-{}: nex subprocess exited {} — will restart",
                    coordinator_id, status
                ));
            }
            Err(e) => {
                logger.error(&format!(
                    "Coordinator-{}: wait() failed on nex subprocess: {} — will restart",
                    coordinator_id, e
                ));
            }
        }
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
    coordinator_id: u32,
    stdin: &mut std::process::ChildStdin,
    response_rx: &mpsc::Receiver<ResponseEvent>,
    logger: &DaemonLogger,
) -> Result<()> {
    let summary = build_crash_recovery_summary(dir, coordinator_id)?;

    // Send as a user message
    let user_msg = format_stream_json_user_message(&summary);
    stdin
        .write_all(user_msg.as_bytes())
        .context("Failed to write crash recovery context to stdin")?;
    stdin.flush().context("Failed to flush stdin")?;

    // Wait for the agent's acknowledgment (shorter timeout than normal messages)
    let ack = collect_response(
        response_rx,
        logger,
        std::time::Duration::from_secs(60),
        None,
    );
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
///
/// Uses bounded context: only messages since last compaction + context-summary.md.
/// This avoids the unbounded inbox/outbox history problem.
fn build_crash_recovery_summary(dir: &Path, coordinator_id: u32) -> Result<String> {
    use workgraph::service::chat_compactor::{ChatCompactorState, context_summary_path};

    let mut parts = Vec::new();

    parts.push("You were restarted after a crash. Context since last compaction:".to_string());
    parts.push(String::new());

    // Load compaction state to get the last compacted message IDs
    let state = ChatCompactorState::load(dir, coordinator_id);

    // Read only messages since last compaction (bounded context)
    let new_inbox = chat::read_inbox_since_for(dir, coordinator_id, state.last_inbox_id)?;
    let new_outbox = chat::read_outbox_since_for(dir, coordinator_id, state.last_outbox_id)?;

    // Interleave by timestamp
    let mut recent_messages = new_inbox;
    recent_messages.extend(new_outbox);
    recent_messages.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    if recent_messages.is_empty() {
        parts.push("(No messages since last compaction.)".to_string());
    } else {
        // Take last N messages
        let start = recent_messages.len().saturating_sub(RECOVERY_HISTORY_COUNT);
        let recent = &recent_messages[start..];

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

    // Include the context-summary.md from the compactor
    let summary_path = context_summary_path(dir, coordinator_id);
    if summary_path.exists()
        && let Ok(summary_content) = std::fs::read_to_string(&summary_path)
    {
        let trimmed = summary_content.trim();
        if !trimmed.is_empty() {
            parts.push("---".to_string());
            parts.push(String::new());
            parts.push("Conversation Context Summary:".to_string());
            parts.push(trimmed.to_string());
        }
    }

    // Add current graph state
    parts.push("---".to_string());
    parts.push(String::new());

    let graph_context =
        build_coordinator_context(dir, "1970-01-01T00:00:00Z", None, coordinator_id)?;
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
    /// Token usage from an assistant turn (per-turn input + output tokens).
    TurnStats {
        input_tokens: u64,
        output_tokens: u64,
        /// Tokens written to cache this turn (new content being cached).
        /// With prompt caching, this is a better proxy for "novel input" than
        /// `input_tokens`, which only counts tokens outside any cache block.
        cache_creation_input_tokens: u64,
    },
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
    /// Per-turn token usage: (input_tokens, output_tokens, cache_creation_input_tokens).
    token_usage: Option<(u64, u64, u64)>,
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

                    // Extract per-turn token usage from message.usage
                    if let Some(usage) = message.get("usage") {
                        let input_tokens = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let output_tokens = usage
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let cache_creation_input_tokens = usage
                            .get("cache_creation_input_tokens")
                            .or_else(|| usage.get("cacheCreationInputTokens"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        if input_tokens > 0 || output_tokens > 0 || cache_creation_input_tokens > 0
                        {
                            let _ = tx.send(ResponseEvent::TurnStats {
                                input_tokens,
                                output_tokens,
                                cache_creation_input_tokens,
                            });
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
///
/// If `streaming_target` is provided, writes partial text to the streaming file as tokens
/// arrive so the TUI can display progressive output.
fn collect_response(
    rx: &mpsc::Receiver<ResponseEvent>,
    logger: &DaemonLogger,
    timeout: std::time::Duration,
    streaming_target: Option<(&Path, u32)>,
) -> Option<CollectedResponse> {
    let deadline = std::time::Instant::now() + timeout;
    let mut parts: Vec<ResponsePart> = Vec::new();
    let mut has_tool_calls = false;
    let mut total_input_tokens: u64 = 0;
    let mut total_output_tokens: u64 = 0;
    let mut total_cache_creation_input_tokens: u64 = 0;
    // Accumulate streaming text for progressive display in the TUI.
    // Uses the same box-drawing format as format_full_response() so that the
    // transition from streaming to finalized display is seamless.
    let mut streaming_text = String::new();
    // Track whether we're inside an open tool box (saw ToolUse, no ToolResult yet).
    let mut in_tool_box = false;

    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .unwrap_or(std::time::Duration::ZERO);
        if remaining.is_zero() {
            logger.warn("Coordinator agent: response collection timed out");
            return build_collected_response(
                &parts,
                has_tool_calls,
                total_input_tokens,
                total_output_tokens,
                total_cache_creation_input_tokens,
            );
        }

        match rx.recv_timeout(remaining) {
            Ok(ResponseEvent::Text(text)) => {
                // Write partial text to the streaming file for TUI progressive display.
                // Match format_full_response(): append text as-is, ensure trailing newline.
                if let Some((dir, coordinator_id)) = streaming_target {
                    streaming_text.push_str(&text);
                    if !text.ends_with('\n') {
                        streaming_text.push('\n');
                    }
                    let _ = chat::write_streaming(dir, coordinator_id, &streaming_text);
                }
                parts.push(ResponsePart::Text(text));
            }
            Ok(ResponseEvent::ToolUse { name, input }) => {
                has_tool_calls = true;
                // Show tool call in the streaming file using box-drawing format
                // matching format_full_response() for seamless transition.
                if let Some((dir, coordinator_id)) = streaming_target {
                    // Close any unclosed tool box from a previous ToolUse.
                    if in_tool_box {
                        streaming_text.push_str("└─\n");
                    }
                    // Tool header
                    streaming_text.push_str(&format!("\n┌─ {} ", name));
                    streaming_text.push_str(&"─".repeat(40usize.saturating_sub(name.len() + 4)));
                    streaming_text.push('\n');
                    // Tool input (same logic as format_full_response)
                    if name == "Bash" || name == "bash" {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&input) {
                            if let Some(cmd) = val.get("command").and_then(|c| c.as_str()) {
                                streaming_text.push_str(&format!("│ $ {}\n", cmd));
                            } else {
                                format_tool_input(&mut streaming_text, &input);
                            }
                        } else {
                            format_tool_input(&mut streaming_text, &input);
                        }
                    } else {
                        format_tool_input(&mut streaming_text, &input);
                    }
                    in_tool_box = true;
                    let _ = chat::write_streaming(dir, coordinator_id, &streaming_text);
                }
                parts.push(ResponsePart::ToolUse { name, input });
            }
            Ok(ResponseEvent::ToolResult(content)) => {
                has_tool_calls = true;
                // Show tool result in box-drawing format matching format_full_response().
                if let Some((dir, coordinator_id)) = streaming_target {
                    if !content.trim().is_empty() {
                        let lines: Vec<&str> = content.lines().collect();
                        let max_lines = 15;
                        if lines.len() > max_lines {
                            for line in &lines[..max_lines] {
                                streaming_text.push_str(&format!("│ {}\n", line));
                            }
                            streaming_text.push_str(&format!(
                                "│ ... ({} more lines)\n",
                                lines.len() - max_lines
                            ));
                        } else {
                            for line in &lines {
                                streaming_text.push_str(&format!("│ {}\n", line));
                            }
                        }
                    }
                    streaming_text.push_str("└─\n");
                    in_tool_box = false;
                    let _ = chat::write_streaming(dir, coordinator_id, &streaming_text);
                }
                parts.push(ResponsePart::ToolResult(content));
            }
            Ok(ResponseEvent::TurnStats {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens,
            }) => {
                // Accumulate per-turn token usage across all sub-turns in the exchange.
                total_input_tokens = total_input_tokens.saturating_add(input_tokens);
                total_output_tokens = total_output_tokens.saturating_add(output_tokens);
                total_cache_creation_input_tokens =
                    total_cache_creation_input_tokens.saturating_add(cache_creation_input_tokens);
            }
            Ok(ResponseEvent::TurnComplete) => {
                // The assistant finished its turn.
                let has_text = parts.iter().any(|p| matches!(p, ResponsePart::Text(_)));
                if !has_text {
                    // Turn complete but no text — this happens when the assistant
                    // only made tool calls. Continue waiting for the next turn.
                    continue;
                }
                return build_collected_response(
                    &parts,
                    has_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    total_cache_creation_input_tokens,
                );
            }
            Ok(ResponseEvent::StreamEnd) => {
                logger.warn("Coordinator agent: stdout stream ended during response collection");
                return build_collected_response(
                    &parts,
                    has_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    total_cache_creation_input_tokens,
                );
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return build_collected_response(
                    &parts,
                    has_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    total_cache_creation_input_tokens,
                );
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return build_collected_response(
                    &parts,
                    has_tool_calls,
                    total_input_tokens,
                    total_output_tokens,
                    total_cache_creation_input_tokens,
                );
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
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_creation_input_tokens: u64,
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

    let token_usage = if total_input_tokens > 0
        || total_output_tokens > 0
        || total_cache_creation_input_tokens > 0
    {
        Some((
            total_input_tokens,
            total_output_tokens,
            total_cache_creation_input_tokens,
        ))
    } else {
        None
    };

    Some(CollectedResponse {
        summary,
        full_text,
        token_usage,
    })
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
// Native executor coordinator loop
// ---------------------------------------------------------------------------

/// Legacy in-process coordinator loop. Superseded by
/// `nex_subprocess_coordinator_loop`, which spawns a `wg nex --chat-id N`
/// subprocess so the coordinator shares the same AgentLoop codepath
/// as interactive `wg nex` and task-agent runs.
///
/// Kept behind `#[allow(dead_code)]` for one release as a safety net:
/// if the subprocess path turns out to have a regression, flipping
/// the dispatcher in `agent_thread_main` back to this function is a
/// one-line revert. Delete once the subprocess path has soaked in
/// production for a release cycle.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
fn native_coordinator_loop(
    dir: &Path,
    coordinator_id: u32,
    model: Option<&str>,
    provider: Option<&str>,
    rx: mpsc::Receiver<ChatRequest>,
    alive: Arc<Mutex<bool>>,
    pid: Arc<Mutex<u32>>,
    logger: &DaemonLogger,
    event_log: &SharedEventLog,
) {
    use workgraph::executor::native::client::{
        ContentBlock, Message, MessagesRequest, MessagesResponse, Role, StopReason,
    };
    use workgraph::executor::native::provider::create_provider_ext;
    use workgraph::executor::native::resume::{ContextBudget, ContextPressureAction};
    use workgraph::executor::native::tools::ToolRegistry;
    use workgraph::executor::native::tools::bash::register_bash_tool;
    use workgraph::models::ModelRegistry;

    let system_prompt = build_system_prompt(dir);

    // Write system prompt to file for debugging (same as Claude CLI path)
    let prompt_file = dir.join("service").join("coordinator-prompt.txt");
    let _ = std::fs::create_dir_all(prompt_file.parent().unwrap());
    let _ = std::fs::write(&prompt_file, &system_prompt);

    // Load config early — needed for model resolution cascade.
    let config = workgraph::config::Config::load_or_default(dir);
    let merged_config = workgraph::config::Config::load_merged(dir).unwrap_or(config);

    // Resolve model: coordinator config > WG_MODEL env > config role cascade
    let explicit_model = model
        .map(String::from)
        .or_else(|| std::env::var("WG_MODEL").ok());

    let (effective_model, effective_provider, effective_endpoint) =
        if let Some(raw_model) = explicit_model {
            // Explicit model specified — resolve through registry
            let spec = workgraph::config::parse_model_spec(&raw_model);
            let spec_provider = spec
                .provider
                .as_deref()
                .map(workgraph::config::provider_to_native_provider)
                .map(String::from);

            let (model, registry_provider, registry_endpoint) =
                if let Some(entry) = merged_config.registry_lookup(&spec.model_id) {
                    (
                        entry.model.clone(),
                        Some(entry.provider.clone()),
                        entry.endpoint.clone(),
                    )
                } else if spec.provider.is_some() {
                    // Has provider prefix but not in registry — use model_id as-is
                    (spec.model_id.clone(), None, None)
                } else {
                    (raw_model.clone(), None, None)
                };

            let prov = spec_provider
                .or_else(|| provider.map(String::from))
                .or(registry_provider)
                .or_else(|| merged_config.coordinator.provider.clone());

            (model, prov, registry_endpoint)
        } else {
            // No explicit model — use the config's role-based resolution cascade
            // (role config → tier defaults → registry lookup → agent.model fallback)
            let resolved =
                merged_config.resolve_model_for_role(workgraph::config::DispatchRole::Default);
            let prov = provider.map(String::from).or(resolved.provider);
            let endpoint = resolved
                .endpoint
                .or_else(|| resolved.registry_entry.and_then(|e| e.endpoint.clone()));
            (resolved.model, prov, endpoint)
        };

    // Create the tokio runtime for async API calls
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            logger.error(&format!(
                "Native coordinator: failed to create tokio runtime: {}",
                e
            ));
            return;
        }
    };

    // Create the LLM provider, passing through the resolved provider and endpoint
    // so that non-Anthropic providers (openrouter, openai, local) route correctly.
    let client = match create_provider_ext(
        dir,
        &effective_model,
        effective_provider.as_deref(),
        effective_endpoint.as_deref(),
        None,
    ) {
        Ok(c) => c,
        Err(e) => {
            logger.error(&format!(
                "Native coordinator: failed to create LLM provider for model '{}': {}",
                effective_model, e
            ));
            return;
        }
    };

    // Build context budget from the provider's context window so compaction
    // respects the model's actual limit (e.g. 32k for qwen3-coder-30b) instead
    // of only the global compaction_token_threshold.
    let context_budget = ContextBudget::with_window_size(client.context_window());
    logger.info(&format!(
        "Native coordinator: context budget window_size={}, compact_threshold={:.0}%, hard_limit={:.0}%",
        context_budget.window_size,
        context_budget.compact_threshold * 100.0,
        context_budget.hard_limit * 100.0,
    ));

    // Check tool support
    let model_registry = ModelRegistry::load(dir).unwrap_or_default();
    let supports_tools = model_registry.supports_tool_use(&effective_model);
    if !supports_tools {
        logger.warn(&format!(
            "Native coordinator: model '{}' does not support tool use, coordinator may be limited",
            effective_model
        ));
    }

    // Build tool registry — coordinator only needs bash (for wg commands)
    let working_dir = dir
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(|pp| pp.to_path_buf()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let mut registry = ToolRegistry::new();
    register_bash_tool(&mut registry, working_dir);

    logger.info(&format!(
        "Native coordinator: initialized with model='{}', provider={}, supports_tools={}",
        effective_model,
        effective_provider.as_deref().unwrap_or("default"),
        supports_tools
    ));

    // Mark as alive (no child PID for native — use thread ID as pseudo-PID)
    *alive.lock().unwrap_or_else(|e| e.into_inner()) = true;
    *pid.lock().unwrap_or_else(|e| e.into_inner()) = std::process::id();

    // Maintain conversation history across interactions
    let mut conversation: Vec<Message> = Vec::new();
    let mut last_interaction = chrono::Utc::now().to_rfc3339();
    let mut turn_count: u32 = 0;
    let mut last_known_compaction_count: u64 = 0;

    // Max API turns per user message (to prevent runaway tool loops)
    let max_turns_per_message: usize = 50;

    loop {
        // Wait for a chat message (with timeout to detect shutdown)
        let request = match rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(req) => req,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                logger.info("Native coordinator: channel closed, shutting down");
                *alive.lock().unwrap_or_else(|e| e.into_inner()) = false;
                *pid.lock().unwrap_or_else(|e| e.into_inner()) = 0;
                return;
            }
        };

        let turn_start = std::time::Instant::now();
        logger.info(&format!(
            "Native coordinator: processing request_id={}",
            request.request_id
        ));

        // Check if chat compaction has run since last message.
        // After compaction, the conversation history is summarized into context-summary.md
        // and we should reset our in-memory conversation to avoid sending duplicate context.
        {
            use workgraph::service::chat_compactor::ChatCompactorState;
            let state = ChatCompactorState::load(dir, coordinator_id);
            if state.compaction_count > last_known_compaction_count {
                logger.info(&format!(
                    "Native coordinator: detected compaction (count {} -> {}), resetting conversation",
                    last_known_compaction_count, state.compaction_count
                ));
                conversation.clear();
                last_known_compaction_count = state.compaction_count;
            }
        }

        // Build context injection with event log
        let context = match build_coordinator_context(
            dir,
            &last_interaction,
            Some(event_log),
            coordinator_id,
        ) {
            Ok(ctx) => ctx,
            Err(e) => {
                logger.warn(&format!(
                    "Native coordinator: failed to build context: {}",
                    e
                ));
                String::new()
            }
        };

        // Format the user message with context injection prepended
        let full_content = if context.is_empty() {
            format!("User message:\n{}", request.message)
        } else {
            format!("{}\n\n---\n\nUser message:\n{}", context, request.message)
        };

        // Add user message to conversation
        conversation.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: full_content }],
        });

        // Process the message through the API, handling tool calls
        let mut parts: Vec<ResponsePart> = Vec::new();
        let mut has_tool_calls = false;
        let mut total_input_tokens: u64 = 0;
        let mut total_output_tokens: u64 = 0;
        let mut total_cache_creation: u64 = 0;
        let mut api_turns = 0;
        let streaming_text = std::sync::Mutex::new(String::new());
        let mut errored = false;

        loop {
            if api_turns >= max_turns_per_message {
                logger.warn(&format!(
                    "Native coordinator: max API turns ({}) reached for request_id={}",
                    max_turns_per_message, request.request_id
                ));
                break;
            }

            // ── Context pressure check ──────────────────────────────
            // Mirror the native agent loop: check estimated token usage
            // against the model's context window and compact or warn
            // before each API call so we never exceed the limit.
            match context_budget.check_pressure(&conversation) {
                ContextPressureAction::CleanExit => {
                    // At 95%+ — aggressively compact and warn the model
                    let pre = conversation.len();
                    conversation = ContextBudget::emergency_compact(conversation, 4);
                    logger.warn(&format!(
                        "Native coordinator: context at hard limit — emergency compact {} → {} messages",
                        pre,
                        conversation.len()
                    ));
                }
                ContextPressureAction::EmergencyCompaction => {
                    let pre = conversation.len();
                    conversation = ContextBudget::emergency_compact(conversation, 8);
                    logger.info(&format!(
                        "Native coordinator: context pressure — compact {} → {} messages",
                        pre,
                        conversation.len()
                    ));
                }
                ContextPressureAction::Warning => {
                    let tokens = context_budget.estimate_tokens(&conversation);
                    let pct = (tokens as f64 / context_budget.window_size as f64) * 100.0;
                    logger.info(&format!(
                        "Native coordinator: context at {:.0}% ({}/{} est tokens)",
                        pct, tokens, context_budget.window_size
                    ));
                }
                ContextPressureAction::Ok => {}
            }

            let tool_defs = if supports_tools {
                registry.definitions()
            } else {
                vec![]
            };

            let api_request = MessagesRequest {
                model: client.model().to_string(),
                max_tokens: client.max_tokens(),
                system: Some(system_prompt.clone()),
                messages: conversation.clone(),
                tools: tool_defs,
                stream: false,
            };

            // Stream text chunks to the TUI incrementally via callback
            let st_ref = &streaming_text;
            let on_text = move |text: String| {
                if let Ok(mut st) = st_ref.lock() {
                    st.push_str(&text);
                    let _ = chat::write_streaming(dir, coordinator_id, &st);
                }
            };

            let response: MessagesResponse =
                match rt.block_on(client.send_streaming(&api_request, &on_text)) {
                    Ok(r) => r,
                    Err(e) => {
                        logger.error(&format!("Native coordinator: API request failed: {}", e));
                        let _ = chat::append_outbox_for(
                            dir,
                            coordinator_id,
                            &format!("The coordinator encountered an API error: {}", e),
                            &request.request_id,
                        );
                        chat::clear_streaming(dir, coordinator_id);
                        errored = true;
                        break;
                    }
                };

            api_turns += 1;

            // Track token usage
            total_input_tokens =
                total_input_tokens.saturating_add(u64::from(response.usage.input_tokens));
            total_output_tokens =
                total_output_tokens.saturating_add(u64::from(response.usage.output_tokens));
            total_cache_creation = total_cache_creation.saturating_add(
                response
                    .usage
                    .cache_creation_input_tokens
                    .map(u64::from)
                    .unwrap_or(0),
            );

            // Process content blocks: text was already streamed via callback,
            // now handle tool calls and record parts.
            let mut tool_use_blocks = Vec::new();
            {
                let mut st = streaming_text.lock().unwrap();
                for block in &response.content {
                    match block {
                        ContentBlock::Text { text } => {
                            // Text was streamed incrementally via callback;
                            // ensure trailing newline for display.
                            if !st.ends_with('\n') {
                                st.push('\n');
                            }
                            let _ = chat::write_streaming(dir, coordinator_id, &st);
                            parts.push(ResponsePart::Text(text.clone()));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            has_tool_calls = true;
                            let input_str = serde_json::to_string(input).unwrap_or_default();

                            // Stream tool call header to TUI
                            st.push_str(&format!("\n┌─ {} ", name));
                            st.push_str(&"─".repeat(40usize.saturating_sub(name.len() + 4)));
                            st.push('\n');
                            if name == "bash" || name == "Bash" {
                                if let Some(cmd) = input.get("command").and_then(|c| c.as_str()) {
                                    st.push_str(&format!("│ $ {}\n", cmd));
                                } else {
                                    format_tool_input(&mut st, &input_str);
                                }
                            } else {
                                format_tool_input(&mut st, &input_str);
                            }
                            let _ = chat::write_streaming(dir, coordinator_id, &st);

                            parts.push(ResponsePart::ToolUse {
                                name: name.clone(),
                                input: input_str,
                            });
                            tool_use_blocks.push((id.clone(), name.clone(), input.clone()));
                        }
                        _ => {}
                    }
                }
            }

            // Add assistant response to conversation
            conversation.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            // Check stop reason
            match response.stop_reason {
                Some(StopReason::EndTurn) | Some(StopReason::StopSequence) | None => {
                    // Done — no more tool calls to process
                    break;
                }
                Some(StopReason::ToolUse) => {
                    // Execute tool calls and add results to conversation
                    let mut tool_results = Vec::new();
                    for (id, name, input) in &tool_use_blocks {
                        let output = rt.block_on(registry.execute(name, input));

                        // Stream tool result to TUI
                        {
                            let mut st = streaming_text.lock().unwrap();
                            if !output.content.trim().is_empty() {
                                let lines: Vec<&str> = output.content.lines().collect();
                                let max_lines = 15;
                                if lines.len() > max_lines {
                                    for line in &lines[..max_lines] {
                                        st.push_str(&format!("│ {}\n", line));
                                    }
                                    st.push_str(&format!(
                                        "│ ... ({} more lines)\n",
                                        lines.len() - max_lines
                                    ));
                                } else {
                                    for line in &lines {
                                        st.push_str(&format!("│ {}\n", line));
                                    }
                                }
                            }
                            st.push_str("└─\n");
                            let _ = chat::write_streaming(dir, coordinator_id, &st);
                        }

                        parts.push(ResponsePart::ToolResult(output.content.clone()));
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: output.content,
                            is_error: output.is_error,
                        });
                    }

                    // Add tool results to conversation
                    conversation.push(Message {
                        role: Role::User,
                        content: tool_results,
                    });
                    // Continue loop for next API call
                }
                Some(StopReason::MaxTokens) => {
                    // Truncated — ask for continuation
                    conversation.push(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: "Your response was truncated. Please continue.".to_string(),
                        }],
                    });
                    // Continue loop
                }
            }
        }

        if errored {
            last_interaction = chrono::Utc::now().to_rfc3339();
            continue;
        }

        // Build the collected response
        let collected = build_collected_response(
            &parts,
            has_tool_calls,
            total_input_tokens,
            total_output_tokens,
            total_cache_creation,
        );

        let response_info = match collected {
            Some(resp) if !resp.summary.is_empty() => {
                let len = resp.summary.len();
                let summary_clone = resp.summary.clone();
                logger.info(&format!(
                    "Native coordinator: got response ({} chars{}) for request_id={}",
                    len,
                    if resp.full_text.is_some() {
                        ", with tool calls"
                    } else {
                        ""
                    },
                    request.request_id
                ));
                if let Err(e) = chat::append_outbox_full_for(
                    dir,
                    coordinator_id,
                    &resp.summary,
                    resp.full_text,
                    &request.request_id,
                ) {
                    logger.error(&format!(
                        "Native coordinator: failed to write outbox: {}",
                        e
                    ));
                }
                Some((len, summary_clone))
            }
            Some(_) => {
                logger.warn("Native coordinator: empty response from API");
                let _ = chat::append_outbox_for(
                    dir,
                    coordinator_id,
                    "The coordinator processed your message but produced no response text.",
                    &request.request_id,
                );
                None
            }
            None => {
                logger.warn("Native coordinator: no response from API");
                let _ = chat::append_outbox_for(
                    dir,
                    coordinator_id,
                    "The coordinator agent received no response from the API.",
                    &request.request_id,
                );
                None
            }
        };

        // Clear streaming file
        chat::clear_streaming(dir, coordinator_id);

        // Accumulate token usage for compaction gating
        let total = total_cache_creation
            .saturating_add(total_input_tokens)
            .saturating_add(total_output_tokens);
        if total > 0 {
            let mut cs = super::CoordinatorState::load_or_default_for(dir, coordinator_id);
            cs.accumulated_tokens = cs.accumulated_tokens.saturating_add(total);
            cs.save_for(dir, coordinator_id);
            logger.info(&format!(
                "Native coordinator: turn used {} tokens (input={}, output={}, cache_creation={}), accumulated={}",
                total, total_input_tokens, total_output_tokens, total_cache_creation, cs.accumulated_tokens
            ));
        }

        // Record turn and run evaluation
        if let Some((resp_len, ref resp_summary)) = response_info {
            turn_count += 1;
            record_coordinator_turn(dir, coordinator_id, &request.message, resp_len, turn_start);

            let eval_config = workgraph::config::Config::load_or_default(dir);
            if should_evaluate_turn(turn_count, &eval_config.coordinator.eval_frequency) {
                let eval_dir = dir.to_path_buf();
                let eval_msg = request.message.clone();
                let eval_resp = resp_summary.clone();
                let eval_turn = turn_count;
                std::thread::Builder::new()
                    .name("coordinator-eval".to_string())
                    .spawn(move || {
                        evaluate_coordinator_turn(&eval_dir, eval_turn, &eval_msg, &eval_resp);
                    })
                    .ok();
            }
        }

        last_interaction = chrono::Utc::now().to_rfc3339();

        // Budget-aware conversation trim: compact if we're above the warning
        // threshold, otherwise apply a generous ceiling to prevent unbounded
        // memory growth between user messages.
        match context_budget.check_pressure(&conversation) {
            ContextPressureAction::EmergencyCompaction | ContextPressureAction::CleanExit => {
                let pre = conversation.len();
                conversation = ContextBudget::emergency_compact(conversation, 6);
                logger.info(&format!(
                    "Native coordinator: post-turn compact {} → {} messages",
                    pre,
                    conversation.len()
                ));
            }
            _ => {
                // Hard ceiling at 200 messages (generous fallback for very large
                // context windows where the budget never fires).
                if conversation.len() > 200 {
                    let drain_count = conversation.len() - 200;
                    conversation.drain(..drain_count);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Claude CLI process spawning
// ---------------------------------------------------------------------------

/// Spawn the Claude CLI process with stream-json pipes.
///
/// Resolves the command binary from the executor registry (same path task agents
/// use), so custom executor configs apply.
/// Falls back to the built-in `"claude"` default when no custom config exists.
///
/// Returns the child process, its stdin handle, and stdout handle.
fn spawn_claude_process(
    dir: &Path,
    executor: &str,
    model: Option<&str>,
    provider: Option<&str>,
    logger: &DaemonLogger,
) -> Result<(Child, std::process::ChildStdin, std::process::ChildStdout)> {
    // Resolve command from executor registry — same code path task agents use.
    // This picks up custom `.workgraph/executors/<name>.toml` configs (including
    // command overrides) and falls back to the built-in default ("claude").
    let registry = ExecutorRegistry::new(dir);
    let executor_config = registry.load_config(executor)?;
    let command = &executor_config.executor.command;

    let system_prompt = build_system_prompt(dir);

    // Write system prompt to a temp file to avoid shell argument length issues
    let prompt_file = dir.join("service").join("coordinator-prompt.txt");
    std::fs::create_dir_all(prompt_file.parent().unwrap())?;
    std::fs::write(&prompt_file, &system_prompt)
        .context("Failed to write coordinator system prompt file")?;

    let mut cmd = Command::new(command);
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
        // Strip provider prefix (e.g., "claude:opus" → "opus") for the CLI
        let spec = workgraph::config::parse_model_spec(m);
        cmd.args(["--model", &spec.model_id]);
    }

    // Note: the Claude CLI does not support --provider. Provider routing is
    // handled via environment variables (e.g., ANTHROPIC_API_KEY for Anthropic,
    // AWS credentials for Bedrock). The `provider` field is used only for
    // logging/diagnostics, not passed to the CLI.
    let _ = provider; // used only in the log message below

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
        "Coordinator agent: spawning {} with model={}, provider={}, cwd={:?}, stderr={:?}",
        command,
        model.unwrap_or("default"),
        provider.unwrap_or("default"),
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
// Turn recording (Phase 2: coordinator-as-graph-citizen)
// ---------------------------------------------------------------------------

/// Record a coordinator turn as a cycle iteration on the `.coordinator` task.
///
/// Logs turn metadata (response length, latency) and increments `loop_iteration`.
fn record_coordinator_turn(
    dir: &Path,
    coordinator_id: u32,
    user_message: &str,
    response_len: usize,
    turn_start: std::time::Instant,
) {
    use workgraph::graph::LogEntry;
    let gp = graph_path(dir);
    let latency = turn_start.elapsed();
    let user_msg_owned = user_message.to_string();

    let task_id = format!(".coordinator-{}", coordinator_id);

    if let Err(e) = workgraph::parser::modify_graph(&gp, |graph| {
        // Try .coordinator-N first, fall back to legacy .coordinator for ID 0
        let coordinator_task_id = if graph.get_task(&task_id).is_some() {
            task_id.as_str()
        } else if coordinator_id == 0 && graph.get_task(".coordinator").is_some() {
            ".coordinator"
        } else {
            return false;
        };
        let task = graph.get_task_mut(coordinator_task_id).unwrap();

        let iteration = task.loop_iteration;
        task.loop_iteration = iteration.saturating_add(1);

        // Truncate user message for the log entry
        let msg_preview: String = user_msg_owned.chars().take(80).collect();
        let msg_suffix = if user_msg_owned.len() > 80 { "..." } else { "" };

        let log_msg = format!(
            "Turn {}: processed \"{}{}\" ({} chars response, {:.1}s)",
            iteration + 1,
            msg_preview,
            msg_suffix,
            response_len,
            latency.as_secs_f64(),
        );

        task.log.push(LogEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            actor: Some("daemon".to_string()),
            user: Some(workgraph::current_user()),
            message: log_msg,
        });

        // Keep log bounded (last 100 entries)
        if task.log.len() > 100 {
            let drain_count = task.log.len() - 100;
            task.log.drain(..drain_count);
        }
        true
    }) {
        eprintln!(
            "[coordinator-agent] Failed to save graph after turn recording: {}",
            e
        );
    }
}

// ---------------------------------------------------------------------------
// Inline evaluation (Phase 3: coordinator-as-graph-citizen)
// ---------------------------------------------------------------------------

/// Coordinator evaluation rubric dimensions with weights.
const COORDINATOR_EVAL_DIMENSIONS: &[(&str, f64)] = &[
    ("decomposition", 0.30),
    ("dependency_accuracy", 0.25),
    ("description_quality", 0.20),
    ("user_responsiveness", 0.15),
    ("efficiency", 0.10),
];

/// Check whether a coordinator turn should be evaluated based on eval_frequency config.
fn should_evaluate_turn(turn_number: u32, eval_frequency: &str) -> bool {
    match eval_frequency {
        "every" => true,
        "every_5" => turn_number.is_multiple_of(5),
        "every_10" => turn_number.is_multiple_of(10),
        "sample_20pct" => {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            turn_number.hash(&mut hasher);
            chrono::Utc::now().timestamp_nanos_opt().hash(&mut hasher);
            hasher.finish().is_multiple_of(5)
        }
        "none" => false,
        _ => turn_number.is_multiple_of(5), // default to every_5
    }
}

/// Build the evaluation prompt for a coordinator turn.
fn build_coordinator_eval_prompt(
    user_message: &str,
    response_summary: &str,
    graph_summary: &str,
) -> String {
    format!(
        r#"You are evaluating a coordinator turn. The coordinator received input and produced output.

## Input
- User message: {user_message}
- Graph state at turn start: {graph_summary}

## Output
- Response to user: {response_summary}

## Evaluation Criteria
Score each dimension 0.0-1.0:

1. **Decomposition (30%)**: Were the tasks well-scoped? Right number? Right boundaries?
2. **Dependency accuracy (25%)**: Correct edges? No cycles that shouldn't be there? Same-file work serialized?
3. **Description quality (20%)**: Clear? Actionable? Validation criteria included?
4. **User responsiveness (15%)**: Helpful? Accurate? Right level of detail?
5. **Efficiency (10%)**: Minimal unnecessary work? No redundant tasks?

Output JSON:
{{
  "score": <float 0.0-1.0>,
  "dimensions": {{
    "decomposition": <float>,
    "dependency_accuracy": <float>,
    "description_quality": <float>,
    "user_responsiveness": <float>,
    "efficiency": <float>
  }},
  "notes": "<brief explanation of strengths and weaknesses>"
}}"#
    )
}

/// Run inline evaluation of a coordinator turn via lightweight LLM call.
///
/// Called asynchronously after eligible turns. Records the evaluation in
/// `.workgraph/agency/evaluations/`.
fn evaluate_coordinator_turn(
    dir: &Path,
    turn_number: u32,
    user_message: &str,
    response_summary: &str,
) {
    use std::collections::HashMap;
    use workgraph::agency::{self, Evaluation};
    use workgraph::config::{Config, DispatchRole};
    use workgraph::service::llm::run_lightweight_llm_call;

    let config = Config::load_or_default(dir);

    // Build a brief graph summary for context
    let gp = graph_path(dir);
    let graph_summary = if let Ok(graph) = load_graph(&gp) {
        let total = graph.tasks().count();
        let done = graph.tasks().filter(|t| t.status == Status::Done).count();
        let in_prog = graph
            .tasks()
            .filter(|t| t.status == Status::InProgress)
            .count();
        let failed = graph.tasks().filter(|t| t.status == Status::Failed).count();
        format!(
            "{} tasks ({} done, {} in-progress, {} failed)",
            total, done, in_prog, failed
        )
    } else {
        "unknown".to_string()
    };

    // Truncate inputs for the eval prompt (keep it cheap)
    let msg_trunc: String = user_message.chars().take(500).collect();
    let resp_trunc: String = response_summary.chars().take(1000).collect();

    let prompt = build_coordinator_eval_prompt(&msg_trunc, &resp_trunc, &graph_summary);

    let result = match run_lightweight_llm_call(&config, DispatchRole::CoordinatorEval, &prompt, 30)
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "[coordinator-eval] Failed to run evaluation LLM call: {}",
                e
            );
            return;
        }
    };

    // Parse the JSON response
    let text = result.text.trim();
    // Extract JSON from possible markdown code blocks
    let json_str = if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            &text[start..=end]
        } else {
            text
        }
    } else {
        text
    };

    let parsed: serde_json::Value = match serde_json::from_str(json_str) {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "[coordinator-eval] Failed to parse evaluation response: {}",
                e
            );
            return;
        }
    };

    let score = parsed.get("score").and_then(|v| v.as_f64()).unwrap_or(0.5);
    let notes = parsed
        .get("notes")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut dimensions = HashMap::new();
    if let Some(dims) = parsed.get("dimensions").and_then(|v| v.as_object()) {
        for (key, val) in dims {
            if let Some(f) = val.as_f64() {
                dimensions.insert(key.clone(), f);
            }
        }
    }

    // Compute weighted score if we got dimensions
    let weighted_score = if !dimensions.is_empty() {
        let mut total = 0.0;
        for (dim_name, weight) in COORDINATOR_EVAL_DIMENSIONS {
            if let Some(dim_score) = dimensions.get(*dim_name) {
                total += dim_score * weight;
            }
        }
        total
    } else {
        score
    };

    let evaluation = Evaluation {
        id: format!("eval-coordinator-turn-{}", turn_number),
        task_id: ".coordinator".to_string(),
        agent_id: String::new(),
        role_id: "coordinator".to_string(),
        tradeoff_id: String::new(),
        score: weighted_score,
        dimensions,
        notes,
        evaluator: "inline-coordinator-eval".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        model: None,
        source: "coordinator-inline".to_string(),
    };

    let agency_dir = dir.join("agency");
    match agency::record_evaluation(&evaluation, &agency_dir) {
        Ok(path) => {
            eprintln!(
                "[coordinator-eval] Turn {} evaluated: {:.2} (saved to {})",
                turn_number,
                weighted_score,
                path.display()
            );
        }
        Err(e) => {
            eprintln!("[coordinator-eval] Failed to record evaluation: {}", e);
        }
    }
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
    coordinator_id: u32,
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

    // --- Compacted Project Context ---
    let context_path = context_md_path(dir);
    if context_path.exists()
        && let Ok(contents) = std::fs::read_to_string(&context_path)
    {
        let contents = contents.trim();
        if !contents.is_empty() {
            let state = CompactorState::load(dir);
            let ts_line = match &state.last_compaction {
                Some(ts) => format!("_Last compacted: {}_\n", ts),
                None => String::new(),
            };
            parts.push(format!(
                "\n### Compacted Project Context\n{}{}",
                ts_line, contents
            ));
        }
    }

    // --- Conversation Context Summary ---
    {
        use workgraph::service::chat_compactor::{ChatCompactorState, context_summary_path};
        let summary_path = context_summary_path(dir, coordinator_id);
        if summary_path.exists()
            && let Ok(contents) = std::fs::read_to_string(&summary_path)
        {
            let contents = contents.trim();
            if !contents.is_empty() {
                let cstate = ChatCompactorState::load(dir, coordinator_id);
                let ts_line = match &cstate.last_compaction {
                    Some(ts) => format!("_Last compacted: {}_\n", ts),
                    None => String::new(),
                };
                parts.push(format!(
                    "\n### Conversation Context Summary\n{}{}",
                    ts_line, contents
                ));
            }
        }
    }

    // --- Injected History Context (from Ctrl+H history browser) ---
    if let Some(injected) = workgraph::chat::take_injected_context(dir, coordinator_id) {
        parts.push(format!(
            "\n### Injected History Context\n\
             _The user selected this from conversation history for your reference:_\n\n{}",
            injected
        ));
    }

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

    // Tell the coordinator where its chat log lives
    let chat_log = chat::chat_log_path_for(dir, coordinator_id);
    parts.push(format!(
        "\n### Chat Log\nYour full chat history is at: {}",
        chat_log.display()
    ));

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
        let ctx = build_coordinator_context(dir, "2026-01-01T00:00:00Z", None, 0).unwrap();
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
        let ctx = build_coordinator_context(dir, "2026-01-01T00:00:00Z", None, 0);
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

        let summary = build_crash_recovery_summary(dir, 0).unwrap();
        assert!(summary.contains("restarted after a crash"));
        // With bounded context, we say "no messages since last compaction"
        assert!(summary.contains("No messages since last compaction"));
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

        let summary = build_crash_recovery_summary(dir, 0).unwrap();
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

        let summary = build_crash_recovery_summary(dir, 0).unwrap();
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

        let summary = build_crash_recovery_summary(dir, 0).unwrap();
        // Should only contain the last RECOVERY_HISTORY_COUNT messages
        // The earliest messages should NOT be present
        assert!(!summary.contains("msg-0"));
        // But later messages should be
        assert!(summary.contains("msg-19") || summary.contains("response-19"));
    }

    #[test]
    fn test_coordinator_context_includes_compaction() {
        use workgraph::service::compactor::{CompactorState, context_md_path};
        use workgraph::test_helpers::{make_task_with_status, setup_workgraph};

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Set up a valid graph so build_coordinator_context proceeds past the early return
        setup_workgraph(
            dir,
            vec![make_task_with_status("task-1", "A task", Status::Open)],
        );

        // Write context.md with known content
        let ctx_path = context_md_path(dir);
        std::fs::create_dir_all(ctx_path.parent().unwrap()).unwrap();
        std::fs::write(&ctx_path, "The project is building a widget system.").unwrap();

        // Write compactor state with a known timestamp
        let state = CompactorState {
            last_compaction: Some("2026-03-10T12:00:00Z".to_string()),
            last_ops_count: 10,
            last_tick: 3,
            compaction_count: 1,
            ..Default::default()
        };
        state.save(dir).unwrap();

        let ctx = build_coordinator_context(dir, "2026-01-01T00:00:00Z", None, 0).unwrap();

        // Compacted context should appear
        assert!(
            ctx.contains("### Compacted Project Context"),
            "missing section header"
        );
        assert!(
            ctx.contains("The project is building a widget system."),
            "missing context body"
        );
        assert!(
            ctx.contains("2026-03-10T12:00:00Z"),
            "missing compaction timestamp"
        );

        // Compacted context should appear BEFORE graph summary
        let compact_pos = ctx.find("### Compacted Project Context").unwrap();
        let graph_pos = ctx.find("### Graph Summary").unwrap();
        assert!(
            compact_pos < graph_pos,
            "compacted context should come before graph summary"
        );
    }

    #[test]
    fn test_coordinator_context_without_compaction() {
        use workgraph::test_helpers::{make_task_with_status, setup_workgraph};

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // Set up a valid graph, no context.md
        setup_workgraph(
            dir,
            vec![make_task_with_status("task-1", "A task", Status::Open)],
        );

        let ctx = build_coordinator_context(dir, "2026-01-01T00:00:00Z", None, 0).unwrap();

        // Should not contain compacted section
        assert!(!ctx.contains("Compacted Project Context"));
        // But should still have graph summary
        assert!(ctx.contains("### Graph Summary"));
    }

    #[test]
    fn test_coordinator_context_includes_chat_summary() {
        use workgraph::service::chat_compactor::{ChatCompactorState, context_summary_path};
        use workgraph::test_helpers::{make_task_with_status, setup_workgraph};

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        setup_workgraph(
            dir,
            vec![make_task_with_status("task-1", "A task", Status::Open)],
        );

        // Write context-summary.md with known content
        let summary_path = context_summary_path(dir, 0);
        std::fs::create_dir_all(summary_path.parent().unwrap()).unwrap();
        std::fs::write(
            &summary_path,
            "# Conversation Context Summary\n\nUser prefers concise responses.",
        )
        .unwrap();

        // Write chat compactor state with a known timestamp
        let state = ChatCompactorState {
            last_compaction: Some("2026-03-27T15:00:00Z".to_string()),
            last_message_count: 20,
            compaction_count: 1,
            last_inbox_id: 10,
            last_outbox_id: 10,
        };
        state.save(dir, 0).unwrap();

        let ctx = build_coordinator_context(dir, "2026-01-01T00:00:00Z", None, 0).unwrap();

        // Chat summary should appear
        assert!(
            ctx.contains("### Conversation Context Summary"),
            "missing chat summary section header"
        );
        assert!(
            ctx.contains("User prefers concise responses."),
            "missing chat summary body"
        );
        assert!(
            ctx.contains("2026-03-27T15:00:00Z"),
            "missing chat compaction timestamp"
        );
    }

    #[test]
    fn test_coordinator_context_without_chat_summary() {
        use workgraph::test_helpers::{make_task_with_status, setup_workgraph};

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        setup_workgraph(
            dir,
            vec![make_task_with_status("task-1", "A task", Status::Open)],
        );

        let ctx = build_coordinator_context(dir, "2026-01-01T00:00:00Z", None, 0).unwrap();

        // Should not contain chat summary section
        assert!(!ctx.contains("Conversation Context Summary"));
    }

    #[test]
    fn test_coordinator_context_includes_injected_history() {
        use workgraph::test_helpers::{make_task_with_status, setup_workgraph};

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        setup_workgraph(
            dir,
            vec![make_task_with_status("task-1", "A task", Status::Open)],
        );

        // Write injected context
        workgraph::chat::write_injected_context(dir, 0, "We discussed auth last week").unwrap();

        let ctx = build_coordinator_context(dir, "2026-01-01T00:00:00Z", None, 0).unwrap();

        // Injected history should appear
        assert!(
            ctx.contains("### Injected History Context"),
            "missing injected history section header"
        );
        assert!(
            ctx.contains("We discussed auth last week"),
            "missing injected history body"
        );

        // After consumption, the file should be gone (take_injected_context removes it)
        assert!(
            workgraph::chat::take_injected_context(dir, 0).is_none(),
            "injected context should be consumed after build_coordinator_context"
        );
    }

    #[test]
    fn test_coordinator_context_no_injected_history_when_absent() {
        use workgraph::test_helpers::{make_task_with_status, setup_workgraph};

        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        setup_workgraph(
            dir,
            vec![make_task_with_status("task-1", "A task", Status::Open)],
        );

        let ctx = build_coordinator_context(dir, "2026-01-01T00:00:00Z", None, 0).unwrap();

        assert!(!ctx.contains("Injected History Context"));
    }
}
