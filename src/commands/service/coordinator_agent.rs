//! Persistent coordinator agent: a long-lived LLM session inside the service daemon.
//!
//! The coordinator runs as a `wg nex --chat coordinator-<N> --role
//! coordinator --resume` subprocess. User messages land in
//! `chat/coordinator-<N>/inbox.jsonl`, responses come out of
//! `chat/coordinator-<N>/outbox.jsonl`, and the full conversation is
//! journaled to `chat/coordinator-<N>/conversation.jsonl`. Same paths,
//! same file formats the TUI already reads.
//!
//! Architecture:
//! - The daemon creates a `CoordinatorAgent` on startup.
//! - A supervisor thread owns the subprocess lifecycle: spawn, wait
//!   for exit, respawn with rate limiting on crash.
//! - A forwarder thread drains the legacy `mpsc::Sender<ChatRequest>`
//!   channel (used by `send_message` and heartbeats) into the inbox
//!   so synthetic messages reach the subprocess.
//! - Inbox messages written directly by the TUI bypass the channel
//!   entirely — the subprocess reads them with inotify-driven wake-ups.
//!
//! Event log: a bounded ring buffer (`EventLog`) is still populated by
//! the daemon main thread to record task completions, agent spawns,
//! etc. Older coordinator builds prepended these events to each user
//! message as context; the subprocess-based coordinator lets the LLM
//! pull graph state on demand via its `wg_*` tools instead. The event
//! log stays exposed so future context-injection work can surface it
//! without changing the daemon's recording paths.

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

/// A timestamped event entry. Fields read via `drain_since` — held
/// here for a future context-injection path.
#[derive(Debug, Clone)]
#[allow(dead_code)]
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
    /// Events older than `since` are discarded. Reserved for future
    /// context-injection work; currently exercised only by tests.
    #[allow(dead_code)]
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
    /// Spawn the coordinator agent.
    ///
    /// Starts a supervisor thread that runs `wg nex --chat
    /// coordinator-<N> --role coordinator --resume` as a subprocess.
    /// The subprocess uses the native executor with whatever provider
    /// the model requires (Anthropic, oai-compat, local, etc.) — one
    /// codepath for every coordinator, no matter what LLM sits behind
    /// it. Messages are exchanged through `chat/coordinator-<N>/`
    /// inbox/outbox JSONL files.
    ///
    /// `executor` and `provider` are accepted for back-compat with the
    /// old call sites but only used to annotate the subprocess's env
    /// (`WG_EXECUTOR_TYPE`, `WG_PROVIDER`). The coordinator
    /// implementation itself is the same regardless.
    ///
    /// The `event_log` is shared with the daemon thread — the daemon
    /// records events (task completions, agent spawns, etc.) for
    /// future context-injection surfacing.
    pub fn spawn(
        dir: &Path,
        coordinator_id: u32,
        model: Option<&str>,
        executor: Option<&str>,
        provider: Option<&str>,
        logger: &DaemonLogger,
        event_log: SharedEventLog,
    ) -> Result<Self> {
        let executor = executor.unwrap_or("native");
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

/// Supervisor thread for a coordinator agent.
///
/// After the Claude-CLI / native split collapse, there is exactly one
/// coordinator backend: a `wg nex --chat coordinator-N --role
/// coordinator --resume` subprocess. This function is a thin adapter
/// that hands off to `nex_subprocess_coordinator_loop`. The
/// indirection is kept so `CoordinatorAgent::spawn` doesn't need to
/// know the implementation details.
#[allow(clippy::too_many_arguments)]
fn agent_thread_main(
    dir: &Path,
    coordinator_id: u32,
    model: Option<&str>,
    _executor: &str,
    provider: Option<&str>,
    rx: mpsc::Receiver<ChatRequest>,
    alive: Arc<Mutex<bool>>,
    pid: Arc<Mutex<u32>>,
    logger: &DaemonLogger,
    _event_log: &SharedEventLog,
) {
    nex_subprocess_coordinator_loop(dir, coordinator_id, model, provider, rx, alive, pid, logger);
}

// ---------------------------------------------------------------------------
// nex-subprocess coordinator: the unified path (nex = task = coordinator)
// ---------------------------------------------------------------------------

/// Coordinator implementation backed by a
/// `wg nex --chat coordinator-<N> --role coordinator --resume`
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
            "Coordinator-{}: spawning `wg nex --chat coordinator-{}` subprocess",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_log_records_and_returns_count() {
        let mut log = EventLog::new();
        log.record(Event::TaskCompleted {
            task_id: "t1".into(),
            agent_id: None,
        });
        log.record(Event::TaskFailed {
            task_id: "t2".into(),
            reason: "boom".into(),
        });
        assert_eq!(log.len(), 2);
    }

    #[test]
    fn event_log_drain_since_filters_old_events() {
        let mut log = EventLog::new();
        log.record(Event::TaskCompleted {
            task_id: "before".into(),
            agent_id: None,
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        let cutoff = Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(20));
        log.record(Event::TaskCompleted {
            task_id: "after".into(),
            agent_id: None,
        });
        let drained = log.drain_since(&cutoff);
        assert_eq!(drained.len(), 1);
        match &drained[0].1 {
            Event::TaskCompleted { task_id, .. } => assert_eq!(task_id, "after"),
            _ => panic!("wrong event kind"),
        }
    }
}
