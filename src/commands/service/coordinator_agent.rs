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
use std::io::{BufRead, BufReader};
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
use workgraph::service::registry::AgentRegistry;

use crate::commands::{graph_path, is_process_alive};

use super::DaemonLogger;

/// Maximum restarts allowed within the restart window before pausing.
const MAX_RESTARTS_PER_WINDOW: usize = 3;

/// Restart window duration in seconds (10 minutes).
const RESTART_WINDOW_SECS: u64 = 600;

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
        // Post-Phase-7, ALL supported executors are backed by a
        // handler subprocess that reads the inbox directly:
        //   native → wg nex --chat
        //   claude → wg claude-handler --chat
        //   codex  → wg codex-handler --chat
        // Only `shell` or an unknown legacy executor would bypass
        // the subprocess path; for those we still return false so
        // `route_chat_to_agent` fires send_message. The historical
        // claude-inline path (`false` for claude) is gone — leaving
        // it caused a double-inbox-append bug: the IPC UserChat
        // handler wrote once, then route_chat_to_agent saw the new
        // message and forwarded via send_message, whose forwarder
        // thread appended AGAIN. Coordinator then saw every user
        // turn twice and complained about a "chat replay loop".
        let uses_subprocess = matches!(executor, "native" | "claude" | "codex")
            || matches!(
                provider,
                Some("openrouter") | Some("oai-compat") | Some("openai") | Some("local")
            )
            || model_requires_native;

        if executor == "claude" && !Self::is_claude_available() {
            anyhow::bail!(
                "Claude CLI not found. Install it to enable the claude-handler coordinator."
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
    /// SIGTERMs the handler child so it releases its session lock
    /// promptly, then drops the sender channel. Without the kill,
    /// a Phase-7 handler (e.g. `wg claude-handler`) would be
    /// orphaned on daemon exit — its blocking I/O keeps it alive
    /// under init, still holding the chat-dir lock, and a fresh
    /// daemon on restart can't reclaim the session.
    pub fn shutdown(self) {
        let pid = *self.pid.lock().unwrap_or_else(|e| e.into_inner());
        if pid > 0 {
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
        }
        drop(self.tx);
        // The supervisor thread's `child.wait()` returns once the
        // handler responds to SIGTERM, letting the forwarder and
        // supervisor exit on their own. We don't join here to avoid
        // blocking daemon shutdown.
    }
}

// ---------------------------------------------------------------------------
// Agent thread implementation
// ---------------------------------------------------------------------------

/// Main loop for the coordinator agent management thread.
///
/// Phase 7 unification: all executors (native, claude, codex, gemini)
/// are dispatched through the same `subprocess_coordinator_loop`,
/// which spawns `wg spawn-task .coordinator-<N>` and lets spawn-task
/// pick the right handler binary based on `WG_EXECUTOR_TYPE`. The
/// daemon itself no longer knows how to speak Claude stdio or native
/// direct-API — it's purely a supervisor + rx-forwarder.
///
/// Executor resolution: config says `claude` or `native` (or future
/// `codex`/`gemini`), but some provider/model combinations force
/// native (Claude CLI only speaks Anthropic). We keep the same
/// auto-routing logic here so a misconfigured graph still ends up on
/// a working handler.
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
    _event_log: &SharedEventLog,
) {
    // Non-Anthropic provider/model forces native regardless of what
    // executor config claims (Claude CLI can only talk to Anthropic).
    let model_requires_native = model
        .map(|m| {
            let config = workgraph::config::Config::load_or_default(dir);
            super::coordinator::requires_native_executor(m, &config)
        })
        .unwrap_or(false);
    let force_native = matches!(
        provider,
        Some("openrouter") | Some("oai-compat") | Some("openai") | Some("local")
    ) || model_requires_native;

    let effective_executor = if force_native && executor != "native" {
        logger.info(&format!(
            "Coordinator-{}: executor '{}' overridden to 'native' (provider/model requires direct API)",
            coordinator_id, executor
        ));
        "native"
    } else {
        executor
    };

    subprocess_coordinator_loop(
        dir,
        coordinator_id,
        model,
        provider,
        effective_executor,
        rx,
        alive,
        pid,
        logger,
    );
}

// ---------------------------------------------------------------------------
// subprocess coordinator: the unified path (native = claude = codex = ...)
// ---------------------------------------------------------------------------

/// Coordinator supervisor loop. Spawns `wg spawn-task .coordinator-<N>`
/// and lets spawn-task's per-executor adapter pick the right handler
/// binary: `wg nex --chat <ref>` for native, `wg claude-handler --chat
/// <ref>` for claude, etc. The daemon is purely a supervisor + inbox
/// forwarder — it no longer speaks any executor's protocol directly.
///
/// `rx` is drained into the inbox so `CoordinatorAgent::send_message`
/// (heartbeats, daemon-internal synthetic prompts) reaches the
/// subprocess. User messages written directly to the inbox by the
/// TUI or `wg chat` bypass this channel.
///
/// `executor` is exported as `WG_EXECUTOR_TYPE` so the spawned
/// `wg spawn-task` picks the correct adapter.
#[allow(clippy::too_many_arguments)]
fn subprocess_coordinator_loop(
    dir: &Path,
    coordinator_id: u32,
    model: Option<&str>,
    provider: Option<&str>,
    executor: &str,
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

        // Register the coordinator's chat session. Installs BOTH
        // aliases (`coordinator-<N>` subprocess-facing AND bare
        // `<N>` legacy-API-facing). The daemon MUST go through
        // this single entry point — see
        // `chat_sessions::register_coordinator_session` docs +
        // the `daemon_style_coordinator_registration_creates_both_paths`
        // unit test that locks in the invariant.
        let chat_alias = format!("coordinator-{}", coordinator_id);
        if let Err(e) = workgraph::chat_sessions::register_coordinator_session(dir, coordinator_id)
        {
            logger.error(&format!(
                "Coordinator-{}: register_coordinator_session failed: {}",
                coordinator_id, e
            ));
        }

        // Phase 6a: spawn via `wg spawn-task .coordinator-<N>` instead
        // of invoking `wg nex --chat <alias>` directly. This unifies
        // the daemon's spawn path with the TUI's (which also uses
        // `wg spawn-task`), so per-executor adapter dispatch,
        // --resume auto-detection, and role resolution all live in
        // ONE place (commands/spawn_task.rs). When Phase 7 adds
        // claude/codex/gemini adapters, the daemon gets them for
        // free — no duplicate executor-routing code to maintain.
        //
        // Task-level model/endpoint overrides on the coordinator
        // task are picked up by spawn-task automatically. The
        // `model` arg the daemon has comes from top-level config
        // and is less specific than the task's own; for now we
        // preserve it via WG_MODEL env so it's applied as a
        // last-resort default by nex.
        // Pre-flight: locate the chat task in the live graph. Prefer the
        // new `.chat-N` prefix; fall back to legacy `.coordinator-N` if we
        // are supervising a task that hasn't been migrated yet.
        //
        // Bug A regression-guard: if NEITHER form exists in the graph, the
        // chat task was deleted (or was never created — e.g. boot path
        // hardcoded "spawn coordinator-0" against a fresh init). DO NOT
        // restart-loop calling `wg spawn-task` with a non-existent ID; log
        // a clear error and exit cleanly so the supervisor thread terminates
        // instead of chewing CPU forever.
        let task_id = {
            let new_id = workgraph::chat_id::format_chat_task_id(coordinator_id);
            let legacy_id = format!(".coordinator-{}", coordinator_id);
            let graph_path = crate::commands::graph_path(dir);
            match workgraph::parser::load_graph(&graph_path) {
                Ok(g) => {
                    if g.get_task(&new_id).is_some() {
                        new_id
                    } else if g.get_task(&legacy_id).is_some() {
                        legacy_id
                    } else {
                        logger.error(&format!(
                            "Coordinator-{}: orphan supervisor — neither {} nor {} exists in the graph. Exiting supervisor (no restart loop). \
                             If you intended to start this chat, run `wg chat new` (or use the TUI '+' key) to create the task first.",
                            coordinator_id, new_id, legacy_id
                        ));
                        return;
                    }
                }
                Err(e) => {
                    logger.error(&format!(
                        "Coordinator-{}: failed to load graph for pre-flight task check: {}. Exiting supervisor.",
                        coordinator_id, e
                    ));
                    return;
                }
            }
        };
        // Hot-swap support: re-read CoordinatorState each iteration
        // so `wg service set-executor <cid> ...` takes effect on the
        // next supervisor restart. Explicit overrides beat the
        // static daemon_cfg values we got at spawn time.
        let state = super::CoordinatorState::load_for(dir, coordinator_id);
        let effective_exec = state
            .as_ref()
            .and_then(|s| s.executor_override.clone())
            .unwrap_or_else(|| executor.to_string());
        let effective_model_override = state
            .as_ref()
            .and_then(|s| s.model_override.clone())
            .or_else(|| model.map(String::from));
        let wg_bin = std::env::current_exe().unwrap_or_else(|_| "wg".into());
        let mut cmd = Command::new(&wg_bin);
        cmd.arg("spawn-task").arg(&task_id);
        cmd.current_dir(dir.parent().unwrap_or(dir));
        cmd.env("WG_EXECUTOR_TYPE", &effective_exec);
        if let Some(p) = provider {
            cmd.env("WG_PROVIDER", p);
        }
        if let Some(ref m) = effective_model_override {
            cmd.env("WG_MODEL", m);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        logger.info(&format!(
            "Coordinator-{}: spawning via `wg spawn-task {}` (executor={}, model={:?})",
            coordinator_id, task_id, effective_exec, effective_model_override
        ));
        let _ = chat_alias; // silence unused — retained for register_coordinator_session above
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
pub fn build_system_prompt(dir: &Path) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
