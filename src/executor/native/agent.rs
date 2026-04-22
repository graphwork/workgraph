//! Tool-use loop for the native executor.
//!
//! Manages the conversation lifecycle: sends messages to the API, executes
//! tool calls, and loops until the agent produces a final text response or
//! hits the max-turns limit.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Serialize;

use super::client::{
    ContentBlock, Message, MessagesRequest, MessagesResponse, Role, StopReason, Usage,
};
use super::journal::{EndReason, Journal, JournalEntryKind};
use super::provider::Provider;
use super::resume::{
    self, ContextBudget, ContextPressureAction, ResumeConfig, estimate_agent_overhead,
};
use super::state_injection::StateInjector;
use super::tools::ToolRegistry;
use crate::stream_event::{self, StreamWriter, TotalUsage, TurnUsage};

/// Default number of turns between session summary extractions.
pub const DEFAULT_SUMMARY_INTERVAL_TURNS: usize = 10;

/// Estimate cost in USD from token counts and registry pricing data.
fn estimate_usage_cost(
    entry: &crate::config::ModelRegistryEntry,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
) -> f64 {
    let input_cost = (input_tokens as f64 / 1_000_000.0) * entry.cost_per_input_mtok;
    let output_cost = (output_tokens as f64 / 1_000_000.0) * entry.cost_per_output_mtok;
    let cache_read_cost = if entry.prompt_caching && entry.cache_read_discount > 0.0 {
        (cache_read_input_tokens as f64 / 1_000_000.0)
            * entry.cost_per_input_mtok
            * entry.cache_read_discount
    } else {
        0.0
    };
    let cache_write_cost = if entry.prompt_caching && entry.cache_write_premium > 0.0 {
        (cache_creation_input_tokens as f64 / 1_000_000.0)
            * entry.cost_per_input_mtok
            * entry.cache_write_premium
    } else {
        0.0
    };
    input_cost + output_cost + cache_read_cost + cache_write_cost
}

/// Record of a single tool call.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallRecord {
    pub name: String,
    pub input: serde_json::Value,
    pub output: String,
    pub is_error: bool,
}

/// Result of running the agent loop.
#[derive(Debug, Clone, Serialize)]
pub struct AgentResult {
    pub final_text: String,
    pub turns: usize,
    pub total_usage: Usage,
    pub tool_calls: Vec<ToolCallRecord>,
    /// Exit reason for the loop. Populated from `session_exit_reason`
    /// in run_interactive. Callers should treat anything other than
    /// `"end_turn"`, `"user_quit"`, or `"eof"` as a failure and mark
    /// the driving task accordingly (discovered 2026-04-17: previously,
    /// `"context_limit"` exits silently landed as task.status = Done
    /// on ulivo because the agent wrapper had no signal that the loop
    /// didn't terminate cleanly).
    #[serde(default)]
    pub exit_reason: String,
}

impl AgentResult {
    /// True when the loop terminated cleanly (model said done, user quit,
    /// stdin closed). False for context_limit / max_turns / any other
    /// abnormal exit — caller should mark the driving task failed.
    pub fn terminated_cleanly(&self) -> bool {
        matches!(self.exit_reason.as_str(), "end_turn" | "user_quit" | "eof")
    }
}

/// The main agent loop.
pub struct AgentLoop {
    client: Box<dyn Provider>,
    tools: ToolRegistry,
    system_prompt: String,
    max_turns: usize,
    output_log: PathBuf,
    stream_writer: Option<StreamWriter>,
    /// Whether the model supports tool use. When false, tools are omitted from requests.
    supports_tools: bool,
    /// Optional journal path for conversation persistence.
    journal_path: Option<PathBuf>,
    /// Task ID for journal metadata.
    task_id: Option<String>,
    /// Workgraph directory root (the `.workgraph` dir). Set alongside
    /// task_id to enable the workgraph inbox in interactive mode.
    workgraph_dir: Option<PathBuf>,
    /// Agent identifier used for inbox cursor tracking.
    agent_id: Option<String>,
    /// Whether to attempt resume from an existing journal.
    resume_enabled: bool,
    /// Working directory for stale-state detection during resume.
    working_dir: Option<PathBuf>,
    /// Number of turns between session summary extractions.
    summary_interval_turns: usize,
    /// Path to the agent's session summary file.
    session_summary_path: Option<PathBuf>,
    /// Path to the `.streaming` file for TUI live display of streaming text.
    streaming_file_path: Option<PathBuf>,
    /// Interval between heartbeat events during tool execution.
    heartbeat_interval: Duration,
    /// Context pressure budget derived from the provider's context window.
    context_budget: ContextBudget,
    /// Mid-turn state injector for ephemeral context updates.
    state_injector: Option<StateInjector>,
    /// Registry entry for cost estimation (populated from spawn path).
    registry_entry: Option<crate::config::ModelRegistryEntry>,
    /// Channels oversized tool outputs to disk so the message vec can never
    /// be exploded by a single large tool call. The agent retrieves full
    /// content via `bash` (cat/head/tail/sed/grep) on the handle path.
    tool_output_channeler: Option<super::channel::ToolOutputChanneler>,
    /// REPL verbose mode. When true, emits compaction diagnostics, token
    /// accounting, session-log-path banner, and other infrastructure
    /// telemetry on top of the tool-call action trace. Implies chatty
    /// mode. Defaults to false. Toggled by `-v` / `--verbose` on `wg nex`.
    nex_verbose: bool,
    /// REPL chatty mode. When true, the full tool output content is
    /// echoed under each tool-call line (as the model sees it, capped
    /// at 20 lines / 1600 bytes). When false (default), only a one-line
    /// summary per call is shown. Implied by `nex_verbose`. Toggled by
    /// `-c` / `--chatty` on `wg nex`.
    nex_chatty: bool,
    /// Autonomous mode. When true, the agent loop does NOT prompt for
    /// user input — it auto-continues on ToolUse (the model called a
    /// tool, we execute it and send results back) and auto-exits on
    /// EndTurn (the model is done talking). This is how background
    /// task agents behave: they get one initial message and run to
    /// completion. When false (default), the loop prompts via
    /// rustyline after each EndTurn.
    autonomous: bool,
    /// REPL mode marker. When true, the agent is running inside an
    /// interactive nex REPL where stdout is the human's terminal and
    /// must stay sacred for assistant text. When false (the default,
    /// used by background native-exec agents), the log events are
    /// *also* mirrored to stdout so the wrapper script can capture
    /// them into `output.log` for TUI display.
    nex_repl_mode: bool,
    /// Chat-file I/O surface. When set, the loop reads user input
    /// from `<workgraph>/chat/<id>/inbox.jsonl` instead of stdin,
    /// mirrors streaming output to `chat/<id>/streaming`, and appends
    /// each finalized assistant turn to `chat/<id>/outbox.jsonl`.
    /// This is what makes `wg nex --chat-id N` serve as a coordinator
    /// (and what will eventually replace the hand-rolled
    /// `native_coordinator_loop`).
    /// Pluggable I/O surface. When `None`, the loop uses rustyline
    /// on stdin + stderr for streaming (the legacy default; equivalent
    /// to `TerminalSurface` but lazily constructed so existing tests
    /// that never hit the terminal don't need rustyline init). When
    /// `Some`, all user input and streaming output flow through the
    /// surface — chat file I/O, PTY, test harness, etc.
    surface: Option<Box<dyn super::surface::ConversationSurface>>,

    /// Session reference of the bound chat session, if any. Stored
    /// separately from the surface so slash commands (`/fork`) can
    /// still reach it after `run_interactive` has taken the surface
    /// into a local variable. `None` when no chat surface is installed.
    chat_session_ref: Option<String>,
}

/// Runtime state for the chat-file surface. Owns the inbox reader and
/// tracks the in-flight request id so streaming + outbox writes can
/// tag correctly.
struct ChatSurfaceState {
    reader: super::chat_surface::ChatInboxReader,
    /// Workgraph root dir (`.workgraph/...`), needed for
    /// `chat::append_outbox_ref` which expects the root, not the
    /// per-chat dir.
    workgraph_dir: PathBuf,
    /// Session reference — UUID, alias, or numeric coord id.
    /// Whatever the caller passed to `with_chat_ref`. All chat-dir
    /// path construction resolves via this + filesystem symlinks.
    session_ref: String,
    /// Live transcript of the current turn. Accumulates across ALL
    /// model calls within one user turn — model text chunks, tool
    /// call markers (`> name(args)`), tool outputs, streaming
    /// tool-progress lines — so the TUI's chat pane sees the same
    /// rich transcript `wg nex` shows on stderr, not just the
    /// latest model response. Cleared on new inbox message, flushed
    /// to the outbox on EndTurn. Wrapped in `Arc<Mutex>` so streaming
    /// callbacks (which run on tokio tasks) can append without
    /// borrowing `self`.
    transcript: Arc<Mutex<String>>,
    /// request_id of the inbox entry currently being responded to —
    /// tagged into the outbox entry so the TUI correlates.
    current_request_id: Option<String>,
}

// ConversationSurface impl for ChatSurfaceState. Moves the chat-
// specific rendering (box drawing, transcript accumulation, outbox
// flush) from the agent-loop body into here so the loop can call
// trait methods instead of accessing ChatSurfaceState fields
// directly. Behavior identical to the inline code that lives in
// `run_interactive` — ported verbatim.
#[async_trait]
impl super::surface::ConversationSurface for ChatSurfaceState {
    async fn next_user_input(&mut self) -> Option<super::surface::UserTurn> {
        let entry = self
            .reader
            .next_entry(std::time::Duration::from_millis(250))
            .await?;
        Some(super::surface::UserTurn::with_request_id(
            entry.message,
            entry.request_id,
        ))
    }

    fn on_turn_start(&mut self, request_id: Option<&str>) {
        self.current_request_id = request_id.map(|s| s.to_string());
        // Fresh user turn — reset the transcript and the streaming
        // dotfile so the display starts clean. EndTurn clears these
        // too, but an interrupted or errored prior turn can leave
        // stale content; this is the belt-and-suspenders reset.
        self.transcript.lock().unwrap().clear();
        super::chat_surface::clear_streaming(&self.workgraph_dir, &self.session_ref);
    }

    fn on_turn_end(&mut self) {
        let final_text = {
            let t = self.transcript.lock().unwrap();
            t.clone()
        };
        if !final_text.is_empty()
            && let Some(rid) = self.current_request_id.clone()
            && let Err(e) = super::chat_surface::append_outbox(
                &self.workgraph_dir,
                &self.session_ref,
                &final_text,
                &rid,
            )
        {
            eprintln!("[agent-loop] chat outbox append failed: {} — continuing", e);
        }
        super::chat_surface::clear_streaming(&self.workgraph_dir, &self.session_ref);
        self.transcript.lock().unwrap().clear();
        self.current_request_id = None;
    }

    fn stream_sink(&self) -> Arc<dyn Fn(&str) + Send + Sync> {
        let wg_dir = self.workgraph_dir.clone();
        let sref = self.session_ref.clone();
        let transcript = self.transcript.clone();
        Arc::new(move |text: &str| {
            let mut t = transcript.lock().unwrap();
            t.push_str(text);
            let _ = super::chat_surface::write_streaming(&wg_dir, &sref, &t);
        })
    }

    fn on_tool_start(&mut self, name: &str, input_summary: &str, input: &serde_json::Value) {
        let mut t = self.transcript.lock().unwrap();
        // Close any previous open tool box.
        if t.ends_with("│ \n") || t.contains("┌─ ") && !t.trim_end().ends_with("└─") {
            let needs_close = t
                .rsplit_terminator('\n')
                .find(|l| !l.is_empty())
                .is_some_and(|l| l.starts_with("│"));
            if needs_close {
                t.push_str("└─\n");
            }
        }
        // New tool box header + input line.
        let header_rule = "─".repeat(40usize.saturating_sub(name.len() + 4));
        t.push_str(&format!("\n┌─ {} {}\n", name, header_rule));
        if name == "Bash" || name == "bash" {
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                t.push_str(&format!("│ $ {}\n", cmd));
            } else {
                t.push_str(&format!("│ {}\n", input_summary));
            }
        } else {
            t.push_str(&format!("│ {}\n", input_summary));
        }
        let _ = super::chat_surface::write_streaming(&self.workgraph_dir, &self.session_ref, &t);
    }

    fn on_tool_progress_chunk(&mut self, chunk: &str) {
        let mut t = self.transcript.lock().unwrap();
        for line in chunk.lines() {
            t.push_str("│ ");
            t.push_str(line);
            t.push('\n');
        }
        let _ = super::chat_surface::write_streaming(&self.workgraph_dir, &self.session_ref, &t);
    }

    fn tool_progress_sink(&self) -> Arc<dyn Fn(&str) + Send + Sync> {
        let wg_dir = self.workgraph_dir.clone();
        let sref = self.session_ref.clone();
        let transcript = self.transcript.clone();
        Arc::new(move |chunk: &str| {
            let mut t = transcript.lock().unwrap();
            for line in chunk.lines() {
                t.push_str("│ ");
                t.push_str(line);
                t.push('\n');
            }
            let _ = super::chat_surface::write_streaming(&wg_dir, &sref, &t);
        })
    }

    fn on_tool_end(&mut self, _name: &str, output: &str, is_error: bool, _duration_ms: u64) {
        let mut t = self.transcript.lock().unwrap();
        let body = if is_error {
            format!("× {}", output)
        } else {
            output.to_string()
        };
        let lines: Vec<&str> = body.lines().collect();
        const MAX_LINES: usize = 15;
        if lines.is_empty() {
            t.push_str("│ (no output)\n");
        } else if lines.len() > MAX_LINES {
            for line in &lines[..MAX_LINES] {
                t.push_str(&format!("│ {}\n", line));
            }
            t.push_str(&format!("│ ... ({} more lines)\n", lines.len() - MAX_LINES));
        } else {
            for line in &lines {
                t.push_str(&format!("│ {}\n", line));
            }
        }
        // Close the box.
        t.push_str("└─\n");
        let _ = super::chat_surface::write_streaming(&self.workgraph_dir, &self.session_ref, &t);
    }
}

/// NDJSON log entry types for the output file.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum LogEvent {
    Turn {
        turn: usize,
        role: &'static str,
        content: Vec<ContentBlockLog>,
        usage: Usage,
    },
    ToolCall {
        name: String,
        input: serde_json::Value,
        output: String,
        is_error: bool,
    },
    Result {
        final_text: String,
        turns: usize,
        total_usage: Usage,
    },
    /// `wg nex` REPL session started — first event in a nex-session log.
    SessionStart {
        /// Wall-clock start time (RFC 3339).
        timestamp: String,
        /// Model identifier the session was opened with.
        model: String,
        /// Endpoint name if one was set, else None.
        endpoint: Option<String>,
        /// Working directory when the session started.
        working_dir: String,
    },
    /// User typed a line in the REPL and submitted it (Enter). Logged
    /// BEFORE the agent turn begins, so slash commands are captured
    /// even if they don't produce an LLM round-trip.
    UserInput {
        /// Raw input line (including leading `/` for slash commands).
        text: String,
    },
    /// REPL session ended cleanly. Logged once at the end of
    /// `run_interactive` regardless of exit reason (user quit, max_turns,
    /// context limit, stream error).
    SessionEnd {
        /// Wall-clock end time (RFC 3339).
        timestamp: String,
        /// How many assistant turns ran in this session.
        turns: usize,
        /// Why the session ended: "user_quit", "eof", "max_turns",
        /// "context_limit", "error".
        reason: &'static str,
        /// Cumulative token usage for the whole session.
        total_usage: Usage,
    },
}

/// Simplified content block for logging (avoids duplicating the full enum).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlockLog {
    Text { text: String },
    Thinking { chars: usize },
    ToolUse { id: String, name: String },
    ToolResult { tool_use_id: String, is_error: bool },
}

impl From<&ContentBlock> for ContentBlockLog {
    fn from(block: &ContentBlock) -> Self {
        match block {
            ContentBlock::Text { text } => ContentBlockLog::Text { text: text.clone() },
            ContentBlock::Thinking { thinking, .. } => ContentBlockLog::Thinking {
                chars: thinking.len(),
            },
            ContentBlock::ToolUse { id, name, .. } => ContentBlockLog::ToolUse {
                id: id.clone(),
                name: name.clone(),
            },
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => ContentBlockLog::ToolResult {
                tool_use_id: tool_use_id.clone(),
                is_error: *is_error,
            },
        }
    }
}

impl AgentLoop {
    /// Create a new agent loop.
    pub fn new(
        client: Box<dyn Provider>,
        tools: ToolRegistry,
        system_prompt: String,
        max_turns: usize,
        output_log: PathBuf,
    ) -> Self {
        Self::with_tool_support(client, tools, system_prompt, max_turns, output_log, true)
    }

    /// Create a new agent loop, specifying whether the model supports tool use.
    pub fn with_tool_support(
        client: Box<dyn Provider>,
        tools: ToolRegistry,
        system_prompt: String,
        max_turns: usize,
        output_log: PathBuf,
        supports_tools: bool,
    ) -> Self {
        // Derive stream.jsonl path from output_log (same directory)
        let stream_path = output_log
            .parent()
            .map(|p| p.join(stream_event::STREAM_FILE_NAME));
        let stream_writer = stream_path.map(StreamWriter::new);

        // Derive .streaming path from output_log directory
        let streaming_file_path = output_log.parent().map(|p| p.join(".streaming"));

        // Build context budget from the provider's context window, with overhead
        // from system prompt + tool definitions + completion reservation. This
        // makes pressure thresholds reflect actual API budget usage rather than
        // just message-content length.
        let context_budget = {
            let tool_defs = tools.definitions();
            let overhead =
                estimate_agent_overhead(&system_prompt, &tool_defs, client.max_tokens(), 4.0);
            ContextBudget::with_window_size(client.context_window())
                .with_overhead(overhead)
                .with_model(client.model())
        };

        // Tool output channeler: writes oversized tool outputs to
        // `<agent_dir>/tool-outputs/` and returns a handle. The agent dir is
        // derived from the output_log's parent. If output_log has no parent
        // (unusual), channeling is disabled and outputs pass through.
        let tool_output_channeler = output_log.parent().map(|agent_dir| {
            super::channel::ToolOutputChanneler::new(agent_dir.join("tool-outputs"))
        });

        Self {
            client,
            tools,
            system_prompt,
            max_turns,
            output_log,
            stream_writer,
            supports_tools,
            journal_path: None,
            task_id: None,
            workgraph_dir: None,
            agent_id: None,
            resume_enabled: true,
            working_dir: None,
            summary_interval_turns: DEFAULT_SUMMARY_INTERVAL_TURNS,
            session_summary_path: None,
            streaming_file_path,
            heartbeat_interval: Duration::from_secs(30),
            context_budget,
            state_injector: None,
            registry_entry: None,
            tool_output_channeler,
            nex_verbose: false,
            nex_chatty: false,
            autonomous: false,
            nex_repl_mode: false,
            surface: None,
            chat_session_ref: None,
        }
    }

    /// Enable verbose REPL output in `run_interactive`.
    /// Defaults to quiet. Use `--verbose` / `-v` on `wg nex` to turn on.
    /// Implies chatty mode.
    pub fn with_nex_verbose(mut self, verbose: bool) -> Self {
        self.nex_verbose = verbose;
        if verbose {
            self.nex_chatty = true;
        }
        self
    }

    /// Enable chatty REPL output — echo the full tool output content
    /// under each tool-call line, as the model sees it. Defaults to
    /// false. Use `--chatty` / `-c` on `wg nex` to turn on.
    pub fn with_nex_chatty(mut self, chatty: bool) -> Self {
        self.nex_chatty = chatty;
        self
    }

    /// Enable autonomous mode (background task agents). The loop
    /// auto-continues on ToolUse and auto-exits on EndTurn instead
    /// of prompting for user input. Set by `run()`, not by `nex.rs`.
    pub fn with_autonomous(mut self, autonomous: bool) -> Self {
        self.autonomous = autonomous;
        self
    }

    /// Mark this agent as running inside a nex REPL. Suppresses the
    /// stdout mirror of NDJSON log events so the human's terminal
    /// stays clean (assistant text only). The disk-side session log
    /// is unaffected.
    pub fn with_nex_repl_mode(mut self, repl: bool) -> Self {
        self.nex_repl_mode = repl;
        self
    }

    /// Set the registry entry for cost estimation.
    pub fn with_registry_entry(mut self, entry: crate::config::ModelRegistryEntry) -> Self {
        self.registry_entry = Some(entry);
        self
    }

    /// Set the heartbeat interval for tool execution (default: 30s).
    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    /// Enable mid-turn state injection.
    ///
    /// When configured, the agent loop will check for new messages, graph state
    /// changes, and context pressure before each API call, injecting them as
    /// ephemeral system-reminder blocks that are NOT persisted to the journal.
    pub fn with_state_injection(
        mut self,
        workgraph_dir: PathBuf,
        task_id: String,
        agent_id: String,
    ) -> Self {
        self.state_injector = Some(StateInjector::new(workgraph_dir, task_id, agent_id));
        self
    }

    /// Configure a workgraph message-queue inbox for this agent. When
    /// set, interactive-mode sessions drain pending messages at every
    /// turn boundary via `WorkgraphInbox` (Stage F). Urgent-priority
    /// messages trigger a cooperative cancel so in-flight work aborts
    /// and the message lands as the next user turn.
    pub fn with_workgraph_inbox(
        mut self,
        workgraph_dir: PathBuf,
        task_id: String,
        agent_id: String,
    ) -> Self {
        self.workgraph_dir = Some(workgraph_dir);
        self.task_id = Some(task_id);
        self.agent_id = Some(agent_id);
        self
    }

    /// Set the journal path for conversation persistence.
    pub fn with_journal(mut self, journal_path: PathBuf, task_id: String) -> Self {
        self.journal_path = Some(journal_path);
        self.task_id = Some(task_id);
        self
    }

    /// Enable or disable resume from existing journal.
    /// When enabled (default), the agent will attempt to load a prior conversation
    /// journal and continue from where the previous agent left off.
    pub fn with_resume(mut self, enabled: bool) -> Self {
        self.resume_enabled = enabled;
        self
    }

    /// Set the working directory for stale-state detection during resume.
    pub fn with_working_dir(mut self, working_dir: PathBuf) -> Self {
        self.working_dir = Some(working_dir);
        self
    }

    /// Set the workgraph root directory. Used by file-producing sub-
    /// systems (L0 defense pending buffers, touched-files re-injection
    /// artifact stash, etc.) that need a stable location to write
    /// artifacts that survive the session.
    pub fn with_workgraph_dir(mut self, workgraph_dir: PathBuf) -> Self {
        self.workgraph_dir = Some(workgraph_dir);
        self
    }

    /// Configure the chat-file I/O surface for this agent. When set,
    /// the loop bypasses stdin/stderr and reads/writes the chat
    /// files under `<workgraph>/chat/<session_ref>/` instead.
    ///
    /// `session_ref` can be a UUID, alias (e.g. `coordinator-0`,
    /// `task-foo`), or legacy numeric id — anything that resolves
    /// to a chat dir via the filesystem symlinks installed by
    /// `crate::chat_sessions`.
    ///
    /// Also sets `workgraph_dir`, `journal_path`, and
    /// `session_summary_path` to the session-derived locations so
    /// `--resume` picks up the right journal automatically.
    ///
    /// If `resume_existing` is false, any pre-existing inbox messages
    /// are skipped (we don't process a previous session's queue).
    /// If true, the cursor from the last run is preserved — typically
    /// pair with `with_resume(true)` so conversation history also
    /// restores.
    pub fn with_chat_ref(
        mut self,
        workgraph_dir: PathBuf,
        session_ref: String,
        resume_existing: bool,
    ) -> Self {
        let paths = super::chat_surface::ChatPaths::for_ref(&workgraph_dir, &session_ref);
        if let Err(e) = paths.ensure_dir() {
            eprintln!(
                "[agent-loop] warning: failed to create chat dir {:?}: {}",
                paths.dir, e
            );
        }
        // The cursor file is the source of truth for "what has this
        // session already processed." If it exists, trust it — any
        // restart (crash or clean) resumes from there. If it doesn't,
        // this is a brand-new session and we process the whole
        // inbox from the start (cursor=0). That was NOT the
        // original behavior — it used to `seek_inbox_to_end` on
        // fresh sessions to skip pre-existing queue — but that
        // dropped the user's first message in the common
        // TUI-coordinator race: the TUI writes "hello" to the
        // inbox at time T, the daemon spawns the coordinator
        // subprocess at T+dt, the subprocess seeks past the
        // "hello" and doesn't see it until 60s later when the
        // heartbeat fires. The fresh-session-seek-to-end was the
        // wrong side of that trade-off. Keep the `resume_existing`
        // parameter in the signature for now for API stability;
        // `ignored` here documents it's no-op for a fresh cursor.
        let _ignored_resume_existing = resume_existing;
        match super::chat_surface::ChatInboxReader::new(
            workgraph_dir.clone(),
            session_ref.clone(),
            paths.clone(),
        ) {
            Ok(reader) => {
                // Chat mode owns the journal + summary locations —
                // override unconditionally so `--resume` finds them
                // at the deterministic chat-dir path regardless of
                // what `with_journal(...)` set earlier in the builder.
                self.journal_path = Some(paths.journal.clone());
                self.session_summary_path = Some(paths.session_summary.clone());
                if self.workgraph_dir.is_none() {
                    self.workgraph_dir = Some(workgraph_dir.clone());
                }
                self.chat_session_ref = Some(session_ref.clone());
                self.surface = Some(Box::new(ChatSurfaceState {
                    reader,
                    workgraph_dir,
                    session_ref,
                    transcript: Arc::new(Mutex::new(String::new())),
                    current_request_id: None,
                }));
            }
            Err(e) => {
                eprintln!(
                    "[agent-loop] warning: chat inbox reader init failed: {} — falling back to stdin mode",
                    e
                );
            }
        }
        self
    }

    /// Legacy numeric-id entry point. Equivalent to
    /// `with_chat_ref(dir, chat_id.to_string(), resume_existing)`.
    pub fn with_chat_id(self, workgraph_dir: PathBuf, chat_id: u32, resume_existing: bool) -> Self {
        self.with_chat_ref(workgraph_dir, chat_id.to_string(), resume_existing)
    }

    /// Install an arbitrary `ConversationSurface` implementation.
    ///
    /// This is the plug point for non-chat-file surfaces — a PTY
    /// surface that reads stdin and writes to a master PTY fd, a
    /// test-harness surface that captures turns into a Vec, a
    /// stdio-pipe surface for a subprocess embedding, etc.
    ///
    /// For chat-file I/O, prefer `with_chat_ref` / `with_chat_id` —
    /// they also set journal/summary paths and the chat_session_ref
    /// needed by `/fork`. `with_surface` is for surfaces that don't
    /// correspond to a `<workgraph>/chat/<ref>/` directory.
    pub fn with_surface(mut self, surface: Box<dyn super::surface::ConversationSurface>) -> Self {
        self.surface = Some(surface);
        self
    }

    /// Resolve a directory for writing agent-produced buffer artifacts
    /// (L0 defense rescue buffers, etc.). Prefer the configured
    /// workgraph_dir; fall back to `<tempdir>/wg-nex-buffers` so a
    /// session without a workgraph root still doesn't crash when
    /// trying to stash an oversized tool_use.
    fn workgraph_dir_for_buffers(&self) -> PathBuf {
        self.workgraph_dir
            .clone()
            .unwrap_or_else(|| std::env::temp_dir().join("wg-nex-buffers"))
    }

    /// Set the session summary extraction interval (in turns).
    /// Default is 10 turns. Set to 0 to disable.
    pub fn with_summary_interval(mut self, turns: usize) -> Self {
        self.summary_interval_turns = turns;
        self
    }

    /// Set the session summary file path.
    /// This is typically `.workgraph/agents/<agent-id>/session-summary.md`.
    pub fn with_session_summary_path(mut self, path: PathBuf) -> Self {
        self.session_summary_path = Some(path);
        self
    }

    /// Get the session summary extraction interval (in turns).
    pub fn summary_interval_turns(&self) -> usize {
        self.summary_interval_turns
    }

    /// Get the session summary file path, if configured.
    pub fn session_summary_path(&self) -> Option<&PathBuf> {
        self.session_summary_path.as_ref()
    }

    /// Run the agent loop to completion (autonomous / background mode).
    ///
    /// This is a thin wrapper that sets `autonomous = true` and delegates
    /// to `run_interactive`. All the real loop logic lives in one place.
    pub async fn run(&mut self, initial_message: &str) -> Result<AgentResult> {
        self.autonomous = true;
        self.run_interactive(Some(initial_message)).await
    }

    // ── Session summary helper ───────────────────────────────────────────

    /// Extract and store a final session summary (called at end of agent run).
    fn store_final_summary(&self, messages: &[Message]) {
        if let Some(ref path) = self.session_summary_path {
            let summary = resume::extract_session_summary(messages);
            if let Err(e) = resume::store_session_summary(path, &summary) {
                eprintln!(
                    "[native-agent] Warning: failed to store final session summary: {}",
                    e
                );
            }
        }
    }

    // ── Logging helpers ─────────────────────────────────────────────────

    fn log_turn(&self, turn: usize, response: &MessagesResponse) {
        let event = LogEvent::Turn {
            turn,
            role: "assistant",
            content: response.content.iter().map(ContentBlockLog::from).collect(),
            usage: response.usage.clone(),
        };
        self.write_log_event(&event);
    }

    fn log_tool_call(&self, name: &str, input: &serde_json::Value, output: &str, is_error: bool) {
        let event = LogEvent::ToolCall {
            name: name.to_string(),
            input: input.clone(),
            output: output.to_string(),
            is_error,
        };
        self.write_log_event(&event);
    }

    fn log_result(&self, result: &AgentResult) {
        let event = LogEvent::Result {
            final_text: result.final_text.clone(),
            turns: result.turns,
            total_usage: result.total_usage.clone(),
        };
        self.write_log_event(&event);
    }

    /// Log the start of a `wg nex` REPL session. Called once as the
    /// first write to a session log file.
    fn log_session_start(&self, endpoint: Option<&str>) {
        let working_dir = self
            .working_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unset>".to_string());
        let event = LogEvent::SessionStart {
            timestamp: chrono::Utc::now().to_rfc3339(),
            model: self.client.model().to_string(),
            endpoint: endpoint.map(String::from),
            working_dir,
        };
        self.write_log_event(&event);
    }

    /// Log a user input line submitted at the REPL prompt. Captures
    /// both regular prompts and slash commands so the full trace is
    /// on disk.
    fn log_user_input(&self, text: &str) {
        let event = LogEvent::UserInput {
            text: text.to_string(),
        };
        self.write_log_event(&event);
    }

    /// Log the end of a REPL session with cumulative stats and the
    /// exit reason.
    fn log_session_end(&self, turns: usize, reason: &'static str, total_usage: &Usage) {
        let event = LogEvent::SessionEnd {
            timestamp: chrono::Utc::now().to_rfc3339(),
            turns,
            reason,
            total_usage: total_usage.clone(),
        };
        self.write_log_event(&event);
    }

    fn write_stream_result(&self, success: bool, result: &AgentResult) {
        if let Some(ref sw) = self.stream_writer {
            let cost_usd = self.registry_entry.as_ref().map(|entry| {
                estimate_usage_cost(
                    entry,
                    u64::from(result.total_usage.input_tokens),
                    u64::from(result.total_usage.output_tokens),
                    result.total_usage.cache_read_input_tokens.unwrap_or(0) as u64,
                    result.total_usage.cache_creation_input_tokens.unwrap_or(0) as u64,
                )
            });
            sw.write_result(
                success,
                TotalUsage {
                    input_tokens: u64::from(result.total_usage.input_tokens),
                    output_tokens: u64::from(result.total_usage.output_tokens),
                    cache_read_input_tokens: result
                        .total_usage
                        .cache_read_input_tokens
                        .map(u64::from),
                    cache_creation_input_tokens: result
                        .total_usage
                        .cache_creation_input_tokens
                        .map(u64::from),
                    cost_usd,
                    model: Some(self.client.model().to_string()),
                },
            );
        }
    }

    fn write_log_event(&self, event: &LogEvent) {
        if let Ok(json) = serde_json::to_string(event) {
            // Write to the dedicated NDJSON log file
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.output_log)
            {
                let _ = writeln!(file, "{}", json);
            }
            // Also mirror to stdout so the background-agent wrapper
            // script captures it into output.log for TUI display.
            // Skip this in nex REPL mode — stdout is the human's
            // terminal, the file is the only sink they care about.
            if !self.nex_repl_mode {
                println!("{}", json);
            }
        }
    }

    // ── Context window monitoring ───────────────────────────────────────────

    /// Inject context warnings if approaching context limits.
    ///
    /// Uses the dynamically-configured `context_budget` (thresholds scale with
    /// the model's context window) instead of hardcoded model-name heuristics.
    fn inject_context_warnings(&self, mut messages: Vec<Message>) -> Vec<Message> {
        let estimated_tokens = self.context_budget.estimate_tokens(&messages);
        let window_size = self.context_budget.window_size;
        let usage_pct = estimated_tokens as f64 / window_size as f64;

        if usage_pct > self.context_budget.warning_threshold {
            let warning = format!(
                "<context-warning>\nContext usage at {:.0}% ({}/{}). \
                Consider: (1) wg log progress, (2) complete current subtask, \
                (3) create follow-up tasks for remaining work.\n\
                </context-warning>",
                usage_pct * 100.0,
                estimated_tokens,
                window_size
            );

            messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: warning }],
            });
        }

        messages
    }

    /// Run an interactive multi-turn REPL.
    ///
    /// Like `run()`, but instead of exiting on `EndTurn`, prints the assistant's
    /// text and reads the next user message from stdin. Streams tokens to stdout
    /// as they arrive. The loop exits when the user sends EOF (Ctrl-D) or types
    /// /quit or /exit.
    pub async fn run_interactive(&mut self, initial_message: Option<&str>) -> Result<AgentResult> {
        use rustyline::DefaultEditor;

        let mut messages: Vec<Message> = Vec::new();
        let mut total_usage = Usage::default();
        let mut tool_calls = Vec::new();
        let mut turns: usize = 0;
        let mut consecutive_server_errors: u32 = 0;
        const MAX_CONSECUTIVE_SERVER_ERRORS: u32 = 3;
        // After a cooperative or hard cancel, force the next iteration to
        // prompt the user for fresh input even if the last message in the
        // vec is user-role. Without this the loop simply re-sends the
        // same conversation to the LLM, which re-generates the same
        // response, which the user cancels again — the "Ctrl-C does
        // nothing" bug discovered in live smoke testing (2026-04-17).
        let mut force_fresh_input = false;
        // Compaction tracking for /status display.
        let mut compaction_count: u32 = 0;
        let mut total_tokens_compacted: usize = 0;
        // Why the REPL is exiting — recorded in the SessionEnd log event.
        // The initial value is a defensive fallback; in practice every
        // `break` from the main loop overwrites it before the
        // `log_session_end` call at the end.
        #[allow(unused_assignments)]
        let mut session_exit_reason: &'static str = "eof";

        // Cancel token drives all interruption signals in this session.
        // Single Ctrl-C → cooperative cancel (let current tool finish).
        // Double Ctrl-C within DOUBLE_TAP_WINDOW → hard cancel
        // (SIGKILL subprocess tree, return to prompt now). Interactive
        // sessions only: in autonomous mode the process is supervised
        // externally, not by a local terminal.
        let cancel = super::cancel::CancelToken::new();
        if !self.autonomous {
            cancel.clone().spawn_ctrl_c_listener();
        }

        // Inbox collects user inputs delivered between turn boundaries.
        // In interactive mode with a workgraph task_id + workgraph_dir,
        // we use the file-based WorkgraphInbox so messages sent via
        // `wg msg send <task-id> "..."` from another terminal arrive
        // here at the next boundary. Urgent-priority messages trigger
        // cooperative cancel so in-flight work aborts. Autonomous
        // sessions keep the state-injection path (no inbox here —
        // would double-drain the message queue).
        let mut inbox: Box<dyn super::inbox::AgentInbox> = if !self.autonomous
            && let (Some(wgd), Some(tid), Some(aid)) =
                (&self.workgraph_dir, &self.task_id, &self.agent_id)
        {
            Box::new(super::inbox::WorkgraphInbox::new(
                wgd.clone(),
                tid.clone(),
                aid.clone(),
                cancel.clone(),
            ))
        } else {
            Box::new(super::inbox::InMemoryInbox::new())
        };

        // Track files the agent has touched. Consulted on history-summary
        // compaction to re-inject a fresh view of the most-recent files
        // (see Stage D in docs/design/native-executor-run-loop.md).
        let mut touched_files = super::touched_files::TouchedFiles::new();

        // Emit the session-start event as the first line of the log
        // file so the trace has a clear beginning marker.
        self.log_session_start(None);

        // Open the journal for this session (if configured via
        // `with_journal`). The journal records the FULL replayable
        // message history — Init, every Message (user/assistant/
        // tool-result), ToolExecution, Compaction, End — in the same
        // format background task agents use. This enables resume,
        // fork, replay, and forensic analysis of nex sessions.
        let mut journal = if let Some(ref path) = self.journal_path {
            match Journal::open(path) {
                Ok(j) => Some(j),
                Err(e) => {
                    eprintln!("[nex] Warning: failed to open journal: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Write Init journal entry with session metadata.
        if let Some(ref mut j) = journal {
            let tool_defs = if self.supports_tools {
                self.tools.definitions()
            } else {
                vec![]
            };
            let _ = j.append(JournalEntryKind::Init {
                model: self.client.model().to_string(),
                provider: self.client.name().to_string(),
                system_prompt: self.system_prompt.clone(),
                tools: tool_defs,
                task_id: self.task_id.clone(),
            });
        }

        // Write Init stream event (for TUI display in autonomous mode)
        if let Some(ref sw) = self.stream_writer {
            sw.write_init("native", Some(self.client.model()), None);
        }

        // ── Resume from prior session (autonomous mode) ─────────────
        // When resume is enabled and a journal/session-summary path is
        // configured, attempt to restore conversation state from a prior
        // agent session. This is the same logic as the old `run()` —
        // session summary takes priority over raw journal replay.
        let session_summary = if self.resume_enabled {
            if let Some(ref path) = self.session_summary_path {
                match resume::load_session_summary(path) {
                    Ok(Some(summary)) => {
                        eprintln!(
                            "[native-agent] Loaded session summary ({} words) from {}",
                            summary.split_whitespace().count(),
                            path.display()
                        );
                        Some(summary)
                    }
                    Ok(None) => None,
                    Err(e) => {
                        eprintln!(
                            "[native-agent] Warning: failed to load session summary: {}",
                            e
                        );
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        let resume_data = if self.resume_enabled && session_summary.is_none() {
            if let Some(ref path) = self.journal_path {
                let working_dir = self
                    .working_dir
                    .as_deref()
                    .unwrap_or_else(|| Path::new("."));
                let resume_config = ResumeConfig {
                    context_window_tokens: self.client.context_window(),
                    ..ResumeConfig::default()
                };
                match resume::load_resume_data(path, working_dir, &resume_config) {
                    Ok(Some(data)) => {
                        eprintln!(
                            "[native-agent] Resuming from journal: {} messages, {} stale annotations{}",
                            data.messages.len(),
                            data.stale_annotations.len(),
                            if data.was_compacted {
                                " (compacted)"
                            } else {
                                ""
                            }
                        );
                        Some(data)
                    }
                    Ok(None) => None,
                    Err(e) => {
                        eprintln!("[native-agent] Warning: failed to load resume data: {}", e);
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        // Pre-populate messages from resume data if available
        if let Some(ref summary) = session_summary {
            if let Some(initial_msg) = initial_message {
                let resume_text = format!(
                    "IMPORTANT: This task is being RESUMED from a prior agent session. \
                     Below is a summary of what was accomplished:\n\n{}\n\n---\n\n{}\n\n\
                     [Continue from where the previous agent left off. The summary above \
                     replaces the full conversation history for efficiency.]",
                    summary, initial_msg
                );
                let content = vec![ContentBlock::Text { text: resume_text }];
                if let Some(ref mut j) = journal {
                    let _ = j.append(JournalEntryKind::Message {
                        role: Role::User,
                        content: content.clone(),
                        usage: None,
                        response_id: None,
                        stop_reason: None,
                    });
                }
                messages.push(Message {
                    role: Role::User,
                    content,
                });
            }
        } else if let Some(ref data) = resume_data {
            // Start with the resumed conversation history
            messages = data.messages.clone();

            if let Some(initial_msg) = initial_message {
                let annotation = resume::build_resume_annotation(data);
                let resume_text = format!(
                    "{}\n\n---\n\n{}\n\n[Continuing from prior session. Review the conversation above and pick up where you left off.]",
                    annotation, initial_msg
                );
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text { text: resume_text }],
                });

                if let Some(ref mut j) = journal {
                    let _ = j.append(JournalEntryKind::Message {
                        role: Role::User,
                        content: messages.last().unwrap().content.clone(),
                        usage: None,
                        response_id: None,
                        stop_reason: None,
                    });
                }
            }
        }

        // Rustyline editor for line editing + history.
        // In autonomous mode we never prompt, but we still create the
        // editor so the rest of the code doesn't need Option<Editor>
        // everywhere.
        let mut editor = DefaultEditor::new().context("failed to initialize rustyline editor")?;
        // Persistent history file — survives sessions.
        let history_path = if let Some(home) = std::env::var_os("HOME") {
            std::path::PathBuf::from(home).join(".workgraph-nex-history")
        } else {
            std::path::PathBuf::from(".workgraph-nex-history")
        };
        if !self.autonomous {
            let _ = editor.load_history(&history_path);
        }

        // Take the surface out of self into a local so it can be
        // mutated across awaits without fighting other &mut self
        // borrows inside this long method. Put it back at the end
        // so subsequent calls (if any) see it.
        let mut surface: Option<Box<dyn super::surface::ConversationSurface>> = self.surface.take();

        // (The rustyline read helper is now inlined into
        // `read_next_user_turn` at module level so we can branch on
        // the surface presence before deciding to block on terminal input.)

        // If resume already populated messages (session summary or journal
        // replay), skip the first-input readline — we go straight to the
        // main loop.
        let resumed = session_summary.is_some() || resume_data.is_some();
        if !resumed {
            // Fresh start — get the first user message
            let first_input = if let Some(msg) = initial_message {
                msg.to_string()
            } else {
                match read_next_user_turn(&mut surface, &mut editor).await {
                    Some(line) => {
                        let trimmed = line.trim().to_string();
                        if trimmed.is_empty() {
                            let _ = editor.save_history(&history_path);
                            self.log_session_end(0, "empty_first_input", &total_usage);
                            return Ok(AgentResult {
                                final_text: String::new(),
                                turns: 0,
                                total_usage,
                                tool_calls,
                                exit_reason: "empty_first_input".to_string(),
                            });
                        }
                        let _ = editor.add_history_entry(&trimmed);
                        self.log_user_input(&trimmed);
                        trimmed
                    }
                    None => {
                        let _ = editor.save_history(&history_path);
                        self.log_session_end(0, "eof", &total_usage);
                        return Ok(AgentResult {
                            final_text: String::new(),
                            turns: 0,
                            total_usage,
                            tool_calls,
                            exit_reason: "eof".to_string(),
                        });
                    }
                }
            };

            // Handle slash commands on the first input too, so users can
            // start with `/help` or `/load session.json` from a cold prompt.
            if first_input.starts_with('/') {
                match self
                    .handle_nex_slash_command(
                        &first_input,
                        &mut messages,
                        &total_usage,
                        turns,
                        compaction_count,
                        total_tokens_compacted,
                    )
                    .await
                {
                    NexSlashResult::Quit => {
                        let _ = editor.save_history(&history_path);
                        return Ok(AgentResult {
                            final_text: String::new(),
                            turns: 0,
                            total_usage,
                            tool_calls,
                            exit_reason: "user_quit".to_string(),
                        });
                    }
                    NexSlashResult::Continue => {
                        // Handled; fall through to the main loop which will
                        // prompt for the next input.
                    }
                    NexSlashResult::NotASlashCommand => {
                        // Journal the initial user message
                        let content = vec![ContentBlock::Text {
                            text: first_input.clone(),
                        }];
                        if let Some(ref mut j) = journal {
                            let _ = j.append(JournalEntryKind::Message {
                                role: Role::User,
                                content: content.clone(),
                                usage: None,
                                response_id: None,
                                stop_reason: None,
                            });
                        }
                        messages.push(Message {
                            role: Role::User,
                            content,
                        });
                    }
                }
            } else {
                // Journal the initial user message
                let content = vec![ContentBlock::Text {
                    text: first_input.clone(),
                }];
                if let Some(ref mut j) = journal {
                    let _ = j.append(JournalEntryKind::Message {
                        role: Role::User,
                        content: content.clone(),
                        usage: None,
                        response_id: None,
                        stop_reason: None,
                    });
                }
                messages.push(Message {
                    role: Role::User,
                    content,
                });
            }
        }

        loop {
            // ── Turn boundary ──────────────────────────────────────
            // Every iteration starts here. The single synchronization
            // point for end-of-turn hooks. Stage C will add microcompact
            // here; Stage B wires the cancel + inbox-drain hooks.

            // 0. Cooperative release: another process (typically the
            //    TUI, when a user sends a message in observer mode)
            //    wrote a release marker asking us to exit cleanly at
            //    the next safe point. We're at that point now. Clear
            //    the marker so a successor handler doesn't immediately
            //    re-exit, record the exit reason, break out.
            //    See docs/design/sessions-as-identity.md §Handoff policy.
            if let (Some(wgd), Some(sref)) = (&self.workgraph_dir, &self.chat_session_ref) {
                let chat_dir = wgd.join("chat").join(sref);
                if crate::session_lock::release_requested(&chat_dir) {
                    crate::session_lock::clear_release_marker(&chat_dir);
                    if !self.autonomous {
                        eprintln!(
                            "\x1b[2m[nex] release requested — exiting cleanly at turn boundary\x1b[0m"
                        );
                    }
                    session_exit_reason = "release_requested";
                    break;
                }
            }

            // 1. Hard cancel: SIGKILL the subprocess tree so any bash /
            //    chrome / curl children from the interrupted tool die
            //    immediately. Descendants detached with setsid/nohup
            //    are still reachable via /proc (we respect the Unix
            //    semantic — genuinely init-reparented processes are
            //    outside our reach by design).
            if cancel.take_hard() {
                let pid = std::process::id();
                crate::service::kill_descendants(pid);
                eprintln!("\n\x1b[31m[nex] Hard cancel — subprocess tree killed.\x1b[0m");
                // Hard implies cooperative — clear both so the next
                // iteration's checks start fresh. Force a fresh
                // readline so the loop doesn't just resend the same
                // conversation to the LLM (the "Ctrl-C does nothing"
                // bug fix).
                cancel.take_cooperative();
                force_fresh_input = true;
                continue;
            }

            // 2. Cooperative cancel: return to prompt, preserve state.
            if cancel.take_cooperative() {
                eprintln!("\n\x1b[33m[nex] Cancelled — returning to prompt.\x1b[0m");
                force_fresh_input = true;
                // If the last message is an assistant turn with
                // unresolved tool_use blocks, synthesize cancelled
                // tool_results so the next LLM call sees a valid
                // message sequence. Otherwise the cancel just drops
                // us at the prompt with the history intact.
                if let Some(last) = messages.last()
                    && last.role == Role::Assistant
                {
                    let unresolved: Vec<_> = last
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .collect();
                    if !unresolved.is_empty() {
                        let results: Vec<_> = unresolved
                            .into_iter()
                            .map(|id| ContentBlock::ToolResult {
                                tool_use_id: id,
                                content: "[cancelled by user before execution]".to_string(),
                                is_error: true,
                            })
                            .collect();
                        messages.push(Message {
                            role: Role::User,
                            content: results,
                        });
                    }
                }
                continue;
            }

            // 3. Drain the inbox. Notes append to the next user turn;
            //    Interrupts do the same but would have already flipped
            //    the cooperative flag when pushed — so by the time we
            //    get here the in-flight work has already aborted. Both
            //    cases land as user messages in the transcript.
            let drained = inbox.drain().await;
            if !drained.is_empty() {
                let mut content = Vec::with_capacity(drained.len());
                for input in drained {
                    content.push(ContentBlock::Text {
                        text: input.text().to_string(),
                    });
                }
                messages.push(Message {
                    role: Role::User,
                    content,
                });
            }

            // 4. Microcompact if above the soft threshold. This is the
            //    always-on variant — runs before context pressure
            //    escalates rather than as an emergency response. On
            //    turns below threshold it's a zero-cost no-op; above
            //    threshold it issues one cheap LLM summary over the
            //    oldest large block (typically a stale tool_result or
            //    long assistant narrative) and replaces it in place.
            //    See docs/design/native-executor-run-loop.md §Stage C.
            let pre_micro = self.context_budget.effective_tokens(&messages);
            let pre_micro_count = messages.len();
            if matches!(
                self.context_budget.check_pressure(&messages),
                ContextPressureAction::Warning | ContextPressureAction::EmergencyCompaction
            ) {
                let (new_messages, bytes_freed) =
                    super::tools::summarize::microcompact_oldest_block(
                        self.client.as_ref(),
                        messages,
                        super::tools::summarize::MICROCOMPACT_KEEP_RECENT_MESSAGES,
                        super::tools::summarize::MICROCOMPACT_MIN_BLOCK_BYTES,
                    )
                    .await;
                messages = new_messages;
                if bytes_freed > 0 {
                    let post_micro = self.context_budget.effective_tokens(&messages);
                    let delta = pre_micro.saturating_sub(post_micro);
                    compaction_count += 1;
                    total_tokens_compacted += delta;
                    if !self.autonomous {
                        eprintln!(
                            "\x1b[2m[microcompact: -{} B (~{} tokens) · {} → {} msgs]\x1b[0m",
                            bytes_freed,
                            delta,
                            pre_micro_count,
                            messages.len()
                        );
                    }
                    if let Some(ref mut j) = journal {
                        let compacted_through_seq = j.seq();
                        let _ = j.append(JournalEntryKind::Compaction {
                            compacted_through_seq,
                            summary: format!(
                                "microcompact: -{} bytes (~{} tokens)",
                                bytes_freed, delta
                            ),
                            original_message_count: pre_micro_count as u32,
                            original_token_count: pre_micro as u32,
                            // Microcompact is structural — it trims tool results
                            // by size — not LLM-summarized. No model used.
                            model_used: None,
                            fallback_reason: None,
                        });
                    }
                }
            }
            // ── end turn boundary ──────────────────────────────────

            if turns >= self.max_turns {
                eprintln!(
                    "\n\x1b[33m[nex] Max turns ({}) reached.\x1b[0m",
                    self.max_turns
                );
                session_exit_reason = "max_turns";
                break;
            }

            // If the last entry isn't a user message (e.g., we just
            // handled a slash command that printed info), OR a recent
            // cancel asked for fresh input, prompt again before the
            // next LLM call.
            let needs_user_input = force_fresh_input
                || messages
                    .last()
                    .map(|m| m.role != Role::User)
                    .unwrap_or(true);
            if needs_user_input {
                force_fresh_input = false;
                match read_next_user_turn(&mut surface, &mut editor).await {
                    Some(line) => {
                        let trimmed = line.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }
                        let _ = editor.add_history_entry(&trimmed);
                        self.log_user_input(&trimmed);
                        if trimmed.starts_with('/') {
                            match self
                                .handle_nex_slash_command(
                                    &trimmed,
                                    &mut messages,
                                    &total_usage,
                                    turns,
                                    compaction_count,
                                    total_tokens_compacted,
                                )
                                .await
                            {
                                NexSlashResult::Quit => {
                                    session_exit_reason = "user_quit";
                                    break;
                                }
                                NexSlashResult::Continue => continue,
                                NexSlashResult::NotASlashCommand => {
                                    messages.push(Message {
                                        role: Role::User,
                                        content: vec![ContentBlock::Text { text: trimmed }],
                                    });
                                }
                            }
                        } else {
                            messages.push(Message {
                                role: Role::User,
                                content: vec![ContentBlock::Text { text: trimmed }],
                            });
                        }
                    }
                    None => {
                        session_exit_reason = "eof";
                        break;
                    }
                }
            }

            // Journal the last message (user input or tool results)
            // before the API call so the journal captures the full
            // input that drove each model turn. In autonomous mode with
            // resume, the initial message was already journaled above.
            if !self.autonomous
                && let Some(ref mut j) = journal
                && let Some(last) = messages.last()
            {
                let _ = j.append(JournalEntryKind::Message {
                    role: last.role,
                    content: last.content.clone(),
                    usage: None,
                    response_id: None,
                    stop_reason: None,
                });
            }

            // ── Mid-turn state injection (ephemeral) ────────────────
            // When a state injector is configured (autonomous task
            // agents), collect dynamic state changes and build
            // request_messages with any injections appended. These are
            // NOT persisted to the journal or to the `messages` vec —
            // they appear once in the API request and then vanish.
            let request_messages = if let Some(ref mut injector) = self.state_injector {
                let pressure_warning = match self.context_budget.check_pressure(&messages) {
                    ContextPressureAction::Warning => {
                        Some(self.context_budget.warning_message(&messages))
                    }
                    _ => None,
                };

                if let Some(injection_text) = injector.collect_injections(pressure_warning) {
                    eprintln!(
                        "[native-agent] Injecting ephemeral state update ({} chars)",
                        injection_text.len()
                    );
                    let mut injected = messages.clone();
                    if let Some(last) = injected.last_mut()
                        && last.role == Role::User
                    {
                        last.content.push(ContentBlock::Text {
                            text: injection_text,
                        });
                    }
                    injected
                } else {
                    messages.clone()
                }
            } else {
                messages.clone()
            };

            // Inject context warnings for OpenRouter models before API call
            let request_messages = self.inject_context_warnings(request_messages);

            let request = MessagesRequest {
                model: self.client.model().to_string(),
                max_tokens: self.client.max_tokens(),
                system: Some(self.system_prompt.clone()),
                messages: request_messages,
                tools: if self.supports_tools {
                    self.tools.definitions()
                } else {
                    vec![]
                },
                stream: false,
            };

            // Build the streaming text callback. In interactive mode,
            // tokens stream to stderr for the human. In autonomous mode,
            // they go to stream.jsonl + .streaming for TUI display.
            // When a chat-surface is set (coordinator mode), ALSO mirror
            // the accumulated text to `<chat-dir>/streaming` so the TUI
            // can tail it for the live assistant view.
            let streaming_file = self.streaming_file_path.clone();
            let stream_writer_clone = self.stream_writer.clone();
            let is_autonomous = self.autonomous;
            // Interactive turn buffer — lets us rewrite the streamed
            // plain text as rendered markdown on EndTurn. Created
            // per-turn so buffers never leak across turns.
            let interactive_turn_buffer: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
            let turn_buf_for_sink = interactive_turn_buffer.clone();
            // Lightning-bolt spinner: shows rainbow `↯` bolts + an
            // elapsed-time counter while we wait for the first
            // streamed token. Cleared when either (a) text starts
            // arriving (first-chunk handoff below), (b) the turn
            // ends for any reason, or (c) we unwind out of the loop
            // via an error / cancel — the RAII guard ensures the
            // spinner stops in ALL paths, not just the happy one.
            // The old implementation left the spinner running when
            // the user hit Ctrl-C during a streaming error retry
            // storm, so the bolts kept interleaving with the retry
            // logs and the "Interrupted" message.
            let spinner_first_chunk = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let spinner_first_chunk_sink = spinner_first_chunk.clone();
            let _spinner_guard = if !is_autonomous && stderr_is_tty() {
                Some(SpinnerGuard::spawn(spinner_first_chunk.clone()))
            } else {
                None
            };
            // Chat-transcript mirror goes through the surface's
            // stream sink — captures transcript buffer + streaming
            // file paths internally; each chunk is appended and the
            // accumulated transcript written to the chat-streaming
            // dotfile the TUI tails.
            let chat_text_sink = surface.as_ref().map(|s| s.stream_sink());
            let on_text = move |text: String| {
                if is_autonomous {
                    if let Some(ref sw) = stream_writer_clone {
                        sw.write_text_chunk(&text);
                    }
                    if let Some(ref path) = streaming_file {
                        let mut accumulated = std::fs::read_to_string(path).unwrap_or_default();
                        accumulated.push_str(&text);
                        let _ = std::fs::write(path, &accumulated);
                    }
                } else {
                    // First-chunk handoff: the spinner thread sees
                    // this atomic flip and clears its own row before
                    // we print any text.
                    if !spinner_first_chunk_sink.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        // Give the spinner a moment to clear; it polls
                        // the flag every 80ms. Skipping this briefly
                        // causes the first chunk to overlay the last
                        // bolt row, which gets erased anyway — harmless
                        // but visually noisier. The sleep is negligible
                        // relative to network latency to the LLM.
                        std::thread::sleep(std::time::Duration::from_millis(90));
                    }
                    if let Ok(mut b) = turn_buf_for_sink.lock() {
                        b.push_str(&text);
                    }
                    eprint!("{}", text);
                    let _ = std::io::stderr().flush();
                }
                if let Some(ref sink) = chat_text_sink {
                    sink(&text);
                }
            };

            // In interactive mode, wrap the streaming call in a
            // Ctrl-C-aware select. In autonomous mode, just await
            // directly (no human to press Ctrl-C).
            let response = if self.autonomous {
                // Autonomous mode: no Ctrl-C handling, but full error
                // recovery with retries and graceful surfacing to model.
                match self.client.send_streaming(&request, &on_text).await {
                    Ok(resp) => resp,
                    Err(e) => {
                        if super::openai_client::is_context_too_long(&e) {
                            eprintln!(
                                "[native-agent] Context too long error — attempting hard emergency compaction and retry"
                            );
                            let pre_tokens = self.context_budget.effective_tokens(&messages);
                            messages = ContextBudget::hard_emergency_compact(messages, 1);
                            let post_tokens = self.context_budget.effective_tokens(&messages);
                            let delta = pre_tokens.saturating_sub(post_tokens);
                            eprintln!(
                                "[native-agent] Hard emergency compacted: ~{} → ~{} tokens (Δ -{}, overhead {} kept, keep_recent_tool_results=1)",
                                pre_tokens, post_tokens, delta, self.context_budget.overhead_tokens,
                            );

                            let retry_max_tokens =
                                std::cmp::max(self.client.max_tokens() / 2, 1024);

                            let retry_streaming_file = self.streaming_file_path.clone();
                            let retry_sw = self.stream_writer.clone();
                            let retry_on_text = move |text: String| {
                                if let Some(ref sw) = retry_sw {
                                    sw.write_text_chunk(&text);
                                }
                                if let Some(ref path) = retry_streaming_file {
                                    let mut acc = std::fs::read_to_string(path).unwrap_or_default();
                                    acc.push_str(&text);
                                    let _ = std::fs::write(path, &acc);
                                }
                            };

                            let retry_request = MessagesRequest {
                                model: self.client.model().to_string(),
                                max_tokens: retry_max_tokens,
                                system: Some(self.system_prompt.clone()),
                                messages: messages.clone(),
                                tools: if self.supports_tools {
                                    self.tools.definitions()
                                } else {
                                    vec![]
                                },
                                stream: false,
                            };

                            match self
                                .client
                                .send_streaming(&retry_request, &retry_on_text)
                                .await
                            {
                                Ok(resp) => resp,
                                Err(retry_err) => {
                                    eprintln!(
                                        "[native-agent] Retry after compaction also failed — clean exit"
                                    );
                                    self.store_final_summary(&messages);
                                    if let Some(ref mut j) = journal {
                                        let _ = j.append(JournalEntryKind::End {
                                            reason: EndReason::MaxTurns,
                                            total_usage: total_usage.clone(),
                                            turns: turns as u32,
                                        });
                                    }
                                    return Err(retry_err).context(
                                        "Context too long — emergency compaction + retry failed",
                                    );
                                }
                            }
                        } else if let Some(api_err) =
                            e.downcast_ref::<super::openai_client::ApiError>()
                        {
                            match api_err.status {
                                401 | 403 => {
                                    eprintln!("[native-agent] Fatal auth error: {}", api_err);
                                    return Err(e);
                                }
                                status if super::openai_client::is_retryable_status(status) => {
                                    consecutive_server_errors += 1;
                                    if consecutive_server_errors > MAX_CONSECUTIVE_SERVER_ERRORS {
                                        eprintln!(
                                            "[native-agent] API error {} — {} consecutive failures, giving up",
                                            status, consecutive_server_errors
                                        );
                                        return Err(e).context(format!(
                                            "API error {} — {} consecutive server errors exceeded limit",
                                            status, consecutive_server_errors
                                        ));
                                    }
                                    eprintln!(
                                        "[native-agent] API error {} after retries — surfacing gracefully to model (attempt {}/{})",
                                        status,
                                        consecutive_server_errors,
                                        MAX_CONSECUTIVE_SERVER_ERRORS
                                    );
                                    let recovery_msg = Message {
                                        role: Role::User,
                                        content: vec![ContentBlock::Text {
                                            text: "There was a temporary issue processing your last request. Please continue with your current task.".to_string(),
                                        }],
                                    };
                                    if let Some(ref mut j) = journal {
                                        let _ = j.append(JournalEntryKind::Message {
                                            role: Role::User,
                                            content: recovery_msg.content.clone(),
                                            usage: None,
                                            response_id: None,
                                            stop_reason: None,
                                        });
                                    }
                                    messages.push(recovery_msg);
                                    continue;
                                }
                                _ => {
                                    return Err(e).context("API request failed");
                                }
                            }
                        } else if is_timeout_error(&e) {
                            consecutive_server_errors += 1;
                            if consecutive_server_errors > MAX_CONSECUTIVE_SERVER_ERRORS {
                                eprintln!(
                                    "[native-agent] Request timeout — {} consecutive failures, giving up",
                                    consecutive_server_errors
                                );
                                return Err(e).context(
                                    "Request timeout — consecutive failures exceeded limit",
                                );
                            }
                            eprintln!(
                                "[native-agent] Request timed out — surfacing gracefully to model (attempt {}/{})",
                                consecutive_server_errors, MAX_CONSECUTIVE_SERVER_ERRORS
                            );
                            let recovery_msg = Message {
                                role: Role::User,
                                content: vec![ContentBlock::Text {
                                    text: "There was a temporary issue processing your last request. Please continue with your current task.".to_string(),
                                }],
                            };
                            if let Some(ref mut j) = journal {
                                let _ = j.append(JournalEntryKind::Message {
                                    role: Role::User,
                                    content: recovery_msg.content.clone(),
                                    usage: None,
                                    response_id: None,
                                    stop_reason: None,
                                });
                            }
                            messages.push(recovery_msg);
                            continue;
                        } else {
                            return Err(e).context("API request failed");
                        }
                    }
                }
            } else {
                // Interactive mode: cooperative cancel aborts the
                // in-flight streaming call. The shared `cancel` token
                // is flipped by the Ctrl-C listener task; we also
                // re-check-and-clear at the next turn boundary so a
                // late signal doesn't get stuck in the flag.
                //
                // Idle watchdog (from claude-code-ts pattern): track
                // the timestamp of the last chunk; if the stream goes
                // quiet for STREAM_IDLE_TIMEOUT_SECS (600s default),
                // abort it. Prevents indefinite hangs on silently
                // dropped connections. Override via env var
                // WG_STREAM_IDLE_TIMEOUT_SECS or --idle-timeout-secs flag.
                let idle_timeout_secs = std::env::var("WG_STREAM_IDLE_TIMEOUT_SECS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(600);
                let last_chunk =
                    std::sync::Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
                let last_chunk_for_callback = last_chunk.clone();
                let inner_on_text = on_text;
                let watched_on_text = move |text: String| {
                    if let Ok(mut t) = last_chunk_for_callback.lock() {
                        *t = std::time::Instant::now();
                    }
                    inner_on_text(text);
                };
                let streaming_future = self.client.send_streaming(&request, &watched_on_text);
                let last_chunk_for_watchdog = last_chunk.clone();
                let idle_watchdog = async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        let elapsed = last_chunk_for_watchdog
                            .lock()
                            .map(|t| t.elapsed())
                            .unwrap_or_default();
                        if elapsed.as_secs() >= idle_timeout_secs {
                            return elapsed;
                        }
                    }
                };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        eprintln!(
                            "\n\x1b[33m[nex] Interrupted — dropping in-flight response.\x1b[0m"
                        );
                        cancel.take_cooperative();
                        force_fresh_input = true;
                        continue;
                    }
                    elapsed = idle_watchdog => {
                        eprintln!(
                            "\n\x1b[33m[nex] Streaming idle for {}s (no chunks) — aborting. \
                             Likely a dropped upstream connection. Next prompt is yours.\x1b[0m",
                            elapsed.as_secs(),
                        );
                        force_fresh_input = true;
                        continue;
                    }
                    res = streaming_future => match res {
                        Ok(resp) => resp,
                        Err(e) => {
                            if super::openai_client::is_context_too_long(&e) {
                                let pre_tokens = self.context_budget.effective_tokens(&messages);
                                messages = ContextBudget::hard_emergency_compact(messages, 1);
                                let post_tokens = self.context_budget.effective_tokens(&messages);
                                if self.nex_verbose {
                                    eprintln!(
                                        "\n\x1b[33m[nex] Context too long — hard compaction: ~{} → ~{} tokens (Δ -{})\x1b[0m",
                                        pre_tokens,
                                        post_tokens,
                                        pre_tokens.saturating_sub(post_tokens),
                                    );
                                }
                                continue;
                            }
                            if let Some(api_err) = e.downcast_ref::<super::openai_client::ApiError>() {
                                if api_err.status == 401 || api_err.status == 403 {
                                    return Err(e);
                                }
                                if super::openai_client::is_retryable_status(api_err.status) {
                                    consecutive_server_errors += 1;
                                    if consecutive_server_errors > MAX_CONSECUTIVE_SERVER_ERRORS {
                                        return Err(e);
                                    }
                                    eprintln!(
                                        "\n\x1b[33m[nex] API error {} — retrying ({}/{})\x1b[0m",
                                        api_err.status,
                                        consecutive_server_errors,
                                        MAX_CONSECUTIVE_SERVER_ERRORS
                                    );
                                    continue;
                                }
                            }
                            if is_timeout_error(&e) {
                                consecutive_server_errors += 1;
                                if consecutive_server_errors > MAX_CONSECUTIVE_SERVER_ERRORS {
                                    return Err(e);
                                }
                                eprintln!("\n\x1b[33m[nex] Request timed out — retrying\x1b[0m");
                                continue;
                            }
                            return Err(e).context("API request failed");
                        }
                    }
                }
            };

            // Successful response — reset consecutive error counter
            consecutive_server_errors = 0;

            // Clean up .streaming file after each turn
            if let Some(ref path) = self.streaming_file_path {
                let _ = std::fs::remove_file(path);
            }

            total_usage.add(&response.usage);
            turns += 1;

            // Log the assistant turn (NDJSON session log)
            self.log_turn(turns, &response);

            // Session summary extraction: after every N turns, extract
            // and store (autonomous mode with summary path configured).
            if self.summary_interval_turns > 0
                && turns.is_multiple_of(self.summary_interval_turns)
                && self.session_summary_path.is_some()
            {
                let summary = resume::extract_session_summary(&messages);
                if let Some(ref path) = self.session_summary_path {
                    if let Err(e) = resume::store_session_summary(path, &summary) {
                        eprintln!(
                            "[native-agent] Warning: failed to store session summary: {}",
                            e
                        );
                    } else {
                        eprintln!(
                            "[native-agent] Session summary extracted at turn {} ({} words)",
                            turns,
                            summary.split_whitespace().count()
                        );
                    }
                }
            }

            // Write Turn stream event (for TUI display)
            if let Some(ref sw) = self.stream_writer {
                let tool_names: Vec<String> = response
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { name, .. } => Some(name.clone()),
                        _ => None,
                    })
                    .collect();
                sw.write_turn(
                    turns as u32,
                    tool_names,
                    Some(TurnUsage {
                        input_tokens: u64::from(response.usage.input_tokens),
                        output_tokens: u64::from(response.usage.output_tokens),
                        cache_read_input_tokens: response
                            .usage
                            .cache_read_input_tokens
                            .map(u64::from),
                        cache_creation_input_tokens: response
                            .usage
                            .cache_creation_input_tokens
                            .map(u64::from),
                        reasoning_tokens: response.usage.reasoning_tokens.map(u64::from),
                    }),
                );
            }

            // Journal the assistant message VERBATIM so the full
            // conversation (including any oversized tool_use args the
            // model emitted) is replayable from the journal. L0 defense
            // below may rewrite the in-context copy of this message
            // with a compact placeholder, but the journal holds the
            // true history.
            if let Some(ref mut j) = journal {
                let _ = j.append(JournalEntryKind::Message {
                    role: Role::Assistant,
                    content: response.content.clone(),
                    usage: Some(response.usage.clone()),
                    response_id: Some(response.id.clone()),
                    stop_reason: response.stop_reason,
                });
            }

            messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            // L0 current-turn defense: scan the just-pushed assistant
            // message for tool_use blocks whose serialized input exceeds
            // the per-call cap. Save oversized inputs to pending
            // buffer files, rewrite the in-context tool_use blocks
            // with compact placeholders, and collect rejections so
            // we can synthesize matching tool_result errors below.
            // Historical compaction (microcompact, summarize-history)
            // protects old turns; L0 protects *this* turn before it
            // enters the context budget.
            let l0_rejections: Vec<super::l0_defense::Rejection> = {
                let last_idx = messages.len() - 1;
                let threshold =
                    super::l0_defense::threshold_for_window(self.client.context_window());
                super::l0_defense::compact_oversized_tool_uses(
                    &mut messages[last_idx],
                    &self.workgraph_dir_for_buffers(),
                    threshold,
                )
            };

            // Whatever the stop reason, the LLM has handed back
            // control — no more need for the "waiting" spinner.
            // Flip the stop flag so the spinner thread clears its
            // row and exits. Idempotent (no-op if already stopped
            // by first-chunk handoff, or if no spinner was started).
            spinner_first_chunk.store(true, std::sync::atomic::Ordering::SeqCst);

            match response.stop_reason {
                Some(StopReason::EndTurn) | Some(StopReason::StopSequence) | None => {
                    if !self.autonomous {
                        let has_text = response
                            .content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::Text { text } if !text.is_empty()));
                        if has_text {
                            eprintln!();
                            // Rewrite the just-streamed plain text as
                            // rendered markdown, only when stderr is a
                            // live TTY. Pipes, redirected files, and
                            // TUI-embedded runs keep the plain stream
                            // (that's what their consumers expect).
                            let buffer = std::mem::take(
                                &mut *interactive_turn_buffer
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner()),
                            );
                            if !buffer.trim().is_empty() && stderr_is_tty() {
                                rerender_markdown_on_stderr(&buffer);
                            }
                        }
                    }

                    // Chat-surface mode: flush the accumulated per-turn
                    // transcript to the outbox tagged with request_id,
                    // clear streaming + transcript + current id. See
                    // `ChatSurfaceState::on_turn_end` for the full
                    // sequence.
                    if let Some(ref mut s) = surface {
                        s.on_turn_end();
                    }

                    // In autonomous mode (task agents), EndTurn means
                    // the model is done — exit the loop. There's no
                    // human to prompt for the next message. This is the
                    // key behavioral difference between interactive and
                    // task-agent use: same loop, different exit
                    // condition on EndTurn.
                    if self.autonomous {
                        session_exit_reason = "end_turn";
                        break;
                    }
                    // In chat-surface mode, DON'T block on stdin next —
                    // fall through to the boundary which will wake the
                    // inbox reader. Skip the rustyline prompt below
                    // when chat is active.
                    // Chat-bound sessions wait for the next inbox turn
                    // via the surface's async next_user_input rather
                    // than blocking on stdin. We detect chat-mode via
                    // chat_session_ref (set by with_chat_ref alongside
                    // the surface). Terminal sessions fall through to
                    // the rustyline prompt below.
                    if self.chat_session_ref.is_some() {
                        continue;
                    }

                    // Add a blank line between the assistant's response
                    // and our next prompt. The readline call handles
                    // rustyline's own display.
                    eprintln!();
                    match read_next_user_turn(&mut surface, &mut editor).await {
                        Some(line) => {
                            let trimmed = line.trim().to_string();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let _ = editor.add_history_entry(&trimmed);
                            self.log_user_input(&trimmed);
                            if trimmed.starts_with('/') {
                                match self
                                    .handle_nex_slash_command(
                                        &trimmed,
                                        &mut messages,
                                        &total_usage,
                                        turns,
                                        compaction_count,
                                        total_tokens_compacted,
                                    )
                                    .await
                                {
                                    NexSlashResult::Quit => {
                                        session_exit_reason = "user_quit";
                                        break;
                                    }
                                    NexSlashResult::Continue => continue,
                                    NexSlashResult::NotASlashCommand => {
                                        messages.push(Message {
                                            role: Role::User,
                                            content: vec![ContentBlock::Text { text: trimmed }],
                                        });
                                    }
                                }
                            } else {
                                messages.push(Message {
                                    role: Role::User,
                                    content: vec![ContentBlock::Text { text: trimmed }],
                                });
                            }
                        }
                        None => {
                            session_exit_reason = "eof";
                            break;
                        }
                    }
                }
                Some(StopReason::ToolUse) => {
                    let tool_use_blocks: Vec<_> = response
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolUse { id, name, input } => {
                                Some((id.clone(), name.clone(), input.clone()))
                            }
                            _ => None,
                        })
                        .collect();

                    // Tool-call previews (interactive mode only — the
                    // human's terminal). In autonomous mode, tool info
                    // goes to stream events and the NDJSON log.
                    if !self.autonomous {
                        for (_, name, input) in &tool_use_blocks {
                            let input_summary = if let Some(cmd) =
                                input.get("command").and_then(|v| v.as_str())
                            {
                                format!("command={}", truncate_for_display(cmd, 120))
                            } else if let Some(path) =
                                input.get("file_path").and_then(|v| v.as_str())
                            {
                                format!("path={}", path)
                            } else if let Some(pat) = input.get("pattern").and_then(|v| v.as_str())
                            {
                                format!("pattern={}", truncate_for_display(pat, 80))
                            } else if let Some(q) = input.get("query").and_then(|v| v.as_str()) {
                                format!("query={}", truncate_for_display(q, 80))
                            } else if let Some(url) = input.get("url").and_then(|v| v.as_str()) {
                                format!("url={}", url)
                            } else {
                                let s = input.to_string();
                                truncate_for_display(&s, 120).to_string()
                            };
                            eprintln!("\x1b[2;36m> {}({})\x1b[0m", name, input_summary);
                            // Mirror tool activity into the chat
                            // transcript using the TUI's expected
                            // box-drawing format
                            // (`┌─ Name ───\n│ ...\n└─`). The TUI's
                            // markdown renderer special-cases these
                            // lines into bordered tool boxes; a
                            // stderr-style `> name(args)` would
                            // render as a markdown blockquote which
                            // looks wrong and loses tool grouping.
                            if let Some(ref mut s) = surface {
                                s.on_tool_start(name, &input_summary, input);
                            }
                        }
                    }

                    // Separate parse-error calls from real calls
                    let mut parse_error_results: Vec<(
                        usize,
                        String,
                        String,
                        serde_json::Value,
                        super::tools::ToolOutput,
                    )> = Vec::new();
                    let mut batch_calls: Vec<(usize, String, super::tools::ToolCall)> = Vec::new();

                    // Map L0 rejections by tool_use_id for O(1) lookup below.
                    let l0_rejected: std::collections::HashMap<
                        &str,
                        &super::l0_defense::Rejection,
                    > = l0_rejections
                        .iter()
                        .map(|r| (r.tool_use_id.as_str(), r))
                        .collect();

                    for (i, (id, name, input)) in tool_use_blocks.iter().enumerate() {
                        if let Some(rej) = l0_rejected.get(id.as_str()) {
                            // L0 defense rejected this tool_use pre-execution —
                            // its args were too big. Don't actually run the tool;
                            // synthesize the explanation as a tool_result error.
                            // The model's next turn sees the rejection + buffer
                            // pointer and can retry with smaller chunks.
                            parse_error_results.push((
                                i,
                                id.clone(),
                                name.clone(),
                                input.clone(),
                                super::tools::ToolOutput {
                                    content: rej.explain(),
                                    is_error: true,
                                },
                            ));
                        } else if input.get("__parse_error").is_some() {
                            let error_msg = input
                                .get("__parse_error")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unknown parse error");
                            let raw_args = input
                                .get("__raw_arguments")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            parse_error_results.push((
                                i,
                                id.clone(),
                                name.clone(),
                                input.clone(),
                                super::tools::ToolOutput {
                                    content: format!(
                                        "ERROR: Tool arguments JSON parse failed: {}. Raw arguments: {}",
                                        error_msg, raw_args
                                    ),
                                    is_error: true,
                                },
                            ));
                        } else {
                            batch_calls.push((
                                i,
                                id.clone(),
                                super::tools::ToolCall {
                                    name: name.clone(),
                                    input: input.clone(),
                                },
                            ));
                        }
                    }

                    // Stream: emit tool_start for all tools
                    if let Some(ref sw) = self.stream_writer {
                        for (_, name, _) in &tool_use_blocks {
                            sw.write_tool_start(name);
                        }
                    }

                    let calls_only: Vec<_> =
                        batch_calls.iter().map(|(_, _, c)| c.clone()).collect();

                    let batch_results = if self.autonomous {
                        // Autonomous mode: heartbeat + streaming tool
                        // output (for TUI live display), no Ctrl-C.

                        // Spawn heartbeat ticker during batch execution
                        let heartbeat_handle = if let Some(ref sw) = self.stream_writer {
                            let sw = sw.clone();
                            let interval = self.heartbeat_interval;
                            Some(tokio::spawn(async move {
                                let mut ticker = tokio::time::interval(interval);
                                ticker.tick().await;
                                loop {
                                    ticker.tick().await;
                                    sw.write_heartbeat();
                                }
                            }))
                        } else {
                            None
                        };

                        // Streaming callback factory for tool output.
                        // Used by long-running tools (bash, deep_research,
                        // map, chunk_map, …) to emit progress chunks.
                        // Chunks get mirrored to: autonomous stream.jsonl,
                        // autonomous `.streaming`, and — now — the chat
                        // transcript, so the TUI sees tool progress lines
                        // in real time rather than a line that only
                        // changes when the model speaks.
                        let streaming_file = self.streaming_file_path.clone();
                        let stream_writer_clone = self.stream_writer.clone();
                        // Tool-progress chunks mirror into the chat
                        // transcript via the surface's sink — which
                        // captures the transcript buffer + streaming
                        // file paths internally and prefixes each
                        // line with `│ ` so it lands inside the open
                        // tool box the TUI draws.
                        let chat_progress_sink = surface.as_ref().map(|s| s.tool_progress_sink());
                        let make_callback = move |_idx: usize| {
                            let sw = stream_writer_clone.clone();
                            let sf = streaming_file.clone();
                            let cps = chat_progress_sink.clone();
                            Box::new(move |text: String| {
                                if let Some(ref writer) = sw {
                                    writer.write_tool_output_chunk("bash", &text);
                                }
                                if let Some(ref path) = sf {
                                    let mut acc = std::fs::read_to_string(path).unwrap_or_default();
                                    acc.push_str(&text);
                                    acc.push('\n');
                                    let _ = std::fs::write(path, &acc);
                                }
                                if let Some(ref sink) = cps {
                                    sink(&text);
                                }
                            }) as super::tools::ToolStreamCallback
                        };

                        let results = self
                            .tools
                            .execute_batch_streaming(
                                &calls_only,
                                super::tools::DEFAULT_MAX_CONCURRENT_TOOLS,
                                make_callback,
                            )
                            .await;

                        // Stop heartbeat ticker
                        if let Some(h) = heartbeat_handle {
                            h.abort();
                        }

                        results
                    } else {
                        // Interactive mode: cooperative cancel interrupts
                        // the tool batch. Stage A treats this as a
                        // select!-level abort — the batch future is
                        // dropped. Stage B will add tree-kill of
                        // spawned subprocesses for double-Ctrl-C.
                        let batch_future = self
                            .tools
                            .execute_batch(&calls_only, super::tools::DEFAULT_MAX_CONCURRENT_TOOLS);
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => {
                                eprintln!(
                                    "\n\x1b[33m[nex] Interrupted during tool execution — returning to prompt.\x1b[0m"
                                );
                                cancel.take_cooperative();
                                let mut interrupted_results = Vec::new();
                                for (id, _name, _input) in &tool_use_blocks {
                                    interrupted_results.push(ContentBlock::ToolResult {
                                        tool_use_id: id.clone(),
                                        content: "[interrupted by user]".to_string(),
                                        is_error: true,
                                    });
                                }
                                messages.push(Message {
                                    role: Role::User,
                                    content: interrupted_results,
                                });
                                force_fresh_input = true;
                                continue;
                            }
                            res = batch_future => res,
                        }
                    };

                    // Merge parse-error results and batch results into
                    // original order
                    let mut all_results: Vec<(
                        usize,
                        String,
                        String,
                        serde_json::Value,
                        super::tools::ToolOutput,
                        u64,
                    )> = Vec::with_capacity(tool_use_blocks.len());

                    for (orig_idx, id, name, input, output) in parse_error_results {
                        all_results.push((orig_idx, id, name, input, output, 0));
                    }
                    for (batch_idx, batch_result) in batch_results.into_iter().enumerate() {
                        let (orig_idx, id, _) = &batch_calls[batch_idx];
                        let input = tool_use_blocks[*orig_idx].2.clone();
                        all_results.push((
                            *orig_idx,
                            id.clone(),
                            batch_result.name,
                            input,
                            batch_result.output,
                            batch_result.duration_ms,
                        ));
                    }
                    all_results.sort_by_key(|(idx, _, _, _, _, _)| *idx);

                    // Process results: streaming, logging, journaling
                    let mut results = Vec::new();
                    for (_, id, name, input, output, duration_ms) in &all_results {
                        // Stream: tool end
                        if let Some(ref sw) = self.stream_writer {
                            sw.write_tool_end(name, output.is_error, *duration_ms);
                        }

                        // Observe touched files for post-compaction
                        // re-injection (Stage D). Silently ignores tools
                        // not in the file-touching allow-list.
                        if !output.is_error {
                            touched_files.observe(name, input);
                        }

                        // Log the tool call (NDJSON session log)
                        self.log_tool_call(name, input, &output.content, output.is_error);

                        // Journal the tool execution
                        if let Some(ref mut j) = journal {
                            let _ = j.append(JournalEntryKind::ToolExecution {
                                tool_use_id: id.clone(),
                                name: name.clone(),
                                input: input.clone(),
                                output: output.content.clone(),
                                is_error: output.is_error,
                                duration_ms: *duration_ms,
                            });
                        }

                        // Interactive-mode display
                        if !self.autonomous {
                            if output.is_error {
                                eprintln!(
                                    "\x1b[31m× {} error: {}\x1b[0m",
                                    name,
                                    summarize_tool_output(&output.content)
                                );
                                if self.nex_chatty {
                                    print_indented_output(&output.content, "\x1b[31m  ", "\x1b[0m");
                                }
                            } else if self.nex_chatty {
                                eprintln!(
                                    "\x1b[2m  → {}\x1b[0m",
                                    summarize_tool_output(&output.content)
                                );
                                print_indented_output(&output.content, "\x1b[2m  ", "\x1b[0m");
                            } else {
                                eprintln!(
                                    "\x1b[2m  → {}\x1b[0m",
                                    summarize_tool_output(&output.content)
                                );
                            }
                            // Mirror the result into the chat
                            // transcript, inside the tool box started
                            // when the tool_use was dispatched. Lines
                            // get the `│ ` prefix; we cap at ~15
                            // lines with a "... N more" tail so the
                            // TUI doesn't get flooded. Full content
                            // is in the journal for post-hoc review.
                            if let Some(ref mut s) = surface {
                                s.on_tool_end(name, &output.content, output.is_error, *duration_ms);
                            }
                        }

                        tool_calls.push(ToolCallRecord {
                            name: name.clone(),
                            input: input.clone(),
                            output: output.content.clone(),
                            is_error: output.is_error,
                        });

                        // Channel oversized outputs to disk before they
                        // enter the message vec (L1).
                        let channeled_content = match &self.tool_output_channeler {
                            Some(c) => c.maybe_channel(name, &output.content),
                            None => output.content.clone(),
                        };

                        results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: channeled_content,
                            is_error: output.is_error,
                        });
                    }

                    // Journal the tool results user message
                    if let Some(ref mut j) = journal {
                        let _ = j.append(JournalEntryKind::Message {
                            role: Role::User,
                            content: results.clone(),
                            usage: None,
                            response_id: None,
                            stop_reason: None,
                        });
                    }

                    messages.push(Message {
                        role: Role::User,
                        content: results,
                    });
                }
                Some(StopReason::MaxTokens) => {
                    messages.push(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: "Your response was truncated. Please continue.".to_string(),
                        }],
                    });
                }
            }

            // ── Context pressure check ──────────────────────────────
            match self.context_budget.check_pressure(&messages) {
                ContextPressureAction::Ok => {}
                ContextPressureAction::Warning => {
                    if self.state_injector.is_some() {
                        // With state injection: warning is handled
                        // ephemerally pre-turn. Just log here.
                        eprintln!(
                            "[native-agent] Context pressure at warning level — will inject ephemerally next turn"
                        );
                    } else if self.autonomous {
                        // Autonomous without state injection: append
                        // warning to messages (non-ephemeral fallback).
                        let warning = self.context_budget.warning_message(&messages);
                        eprintln!("[native-agent] {}", warning);
                        if let Some(last) = messages.last_mut()
                            && last.role == Role::User
                        {
                            last.content.push(ContentBlock::Text { text: warning });
                        }
                    }
                    // Interactive mode: no action on warning (user
                    // controls the conversation)
                }
                ContextPressureAction::EmergencyCompaction => {
                    // Proactive microcompact at the turn boundary
                    // handles most pressure. Reaching this branch means
                    // the in-flight turn pushed us over the threshold
                    // in a single step (e.g. one huge tool result) and
                    // microcompact hasn't had a chance to fire yet. We
                    // fall back to a full-history summary using the
                    // 9-section prompt.
                    //
                    // The replacement is self-describing — the summary
                    // message itself is the signal to the model that
                    // compaction happened. No more "[System note:
                    // compacted]" tax (which grew the context every
                    // time it fired, exactly opposite of its intent).
                    let pre_tokens = self.context_budget.effective_tokens(&messages);
                    let pre_count = messages.len();

                    messages =
                        super::tools::summarize::summarize_history_for_compaction_cancellable(
                            self.client.as_ref(),
                            messages,
                            Some(cancel.clone()),
                        )
                        .await;

                    // Post-compaction file re-injection (Stage D). The
                    // summary preserves narrative; the re-read files
                    // preserve state. The model wakes up with both:
                    // "what happened" + "what does my environment
                    // currently look like". Fresh reads from disk, so
                    // any concurrent modifications show up too.
                    if let Some(markdown) =
                        super::touched_files::reinject_files_markdown(&touched_files)
                    {
                        messages.push(Message {
                            role: Role::User,
                            content: vec![ContentBlock::Text { text: markdown }],
                        });
                    }

                    let post_tokens = self.context_budget.effective_tokens(&messages);
                    let post_count = messages.len();
                    let delta = pre_tokens.saturating_sub(post_tokens);

                    compaction_count += 1;
                    total_tokens_compacted += delta;

                    if self.autonomous {
                        eprintln!(
                            "[native-agent] history-summary compaction: ~{} → ~{} tokens (Δ -{}, {} → {} messages, overhead {} kept)",
                            pre_tokens,
                            post_tokens,
                            delta,
                            pre_count,
                            post_count,
                            self.context_budget.overhead_tokens,
                        );
                    } else {
                        eprintln!(
                            "\x1b[2m[history-summary compacted: ~{} → ~{} tokens (Δ -{}), {} → {} msgs]\x1b[0m",
                            pre_tokens, post_tokens, delta, pre_count, post_count
                        );
                    }

                    // Journal the compaction event. The LLM
                    // summarization already happened inside
                    // `summarize_history_for_compaction_cancellable`
                    // above — if it succeeded, the resulting
                    // `messages[0]` is a user-role block prefixed
                    // "PRIOR CONVERSATION SUMMARY:" with the real
                    // summary text. If it failed, it returned the
                    // original vec unchanged, which means
                    // `post_count == pre_count` and no summary was
                    // produced. Distinguish those cases so resume
                    // logic can see whether the journal captures a
                    // real summary or just a metadata marker.
                    if let Some(ref mut j) = journal {
                        let compacted_through_seq = j.seq();
                        let llm_succeeded = post_count < pre_count;
                        let (summary_text, model_used, fallback_reason) = if llm_succeeded {
                            let first_text = messages
                                .first()
                                .and_then(|m| m.content.first())
                                .and_then(|b| match b {
                                    ContentBlock::Text { text } => Some(text.clone()),
                                    _ => None,
                                })
                                .unwrap_or_else(|| {
                                    format!(
                                        "history-summary compaction ({} → {} msgs, ~{} → ~{} tokens)",
                                        pre_count, post_count, pre_tokens, post_tokens
                                    )
                                });
                            (first_text, Some(self.client.model().to_string()), None)
                        } else {
                            (
                                format!(
                                    "history-summary compaction attempted but returned unchanged ({} msgs, ~{} tokens)",
                                    pre_count, pre_tokens
                                ),
                                None,
                                Some(
                                    "summarize_history returned messages unchanged (LLM or recursive-summary error; see stderr)".to_string(),
                                ),
                            )
                        };
                        let _ = j.append(JournalEntryKind::Compaction {
                            compacted_through_seq,
                            summary: summary_text,
                            original_message_count: pre_count as u32,
                            original_token_count: pre_tokens as u32,
                            model_used,
                            fallback_reason,
                        });
                    }
                }
                ContextPressureAction::CleanExit => {
                    if self.autonomous {
                        eprintln!(
                            "[native-agent] Context at 95%+ capacity — performing clean exit"
                        );
                    } else {
                        eprintln!(
                            "\x1b[33m[nex] Context limit reached. Please start a new session.\x1b[0m"
                        );
                    }
                    self.store_final_summary(&messages);
                    session_exit_reason = "context_limit";
                    break;
                }
            }
        }

        // ── Post-loop cleanup ───────────────────────────────────────

        // Store final session summary
        self.store_final_summary(&messages);

        // Persist readline history to disk on exit (best-effort).
        if !self.autonomous {
            let _ = editor.save_history(&history_path);
        }

        let final_text = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map(|m| {
                m.content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();

        let result = AgentResult {
            final_text,
            turns,
            total_usage: total_usage.clone(),
            tool_calls,
            exit_reason: session_exit_reason.to_string(),
        };

        // Log result and emit stream result event
        self.log_result(&result);
        self.write_stream_result(
            session_exit_reason == "end_turn"
                || session_exit_reason == "user_quit"
                || session_exit_reason == "eof",
            &result,
        );

        // Emit the session-end marker into the session log
        self.log_session_end(turns, session_exit_reason, &total_usage);

        // Close the journal with an End entry.
        if let Some(ref mut j) = journal {
            let reason = match session_exit_reason {
                "user_quit" | "eof" | "end_turn" => EndReason::Complete,
                "max_turns" => EndReason::MaxTurns,
                "context_limit" => EndReason::Error {
                    message: "context limit".to_string(),
                },
                other => EndReason::Error {
                    message: other.to_string(),
                },
            };
            let _ = j.append(JournalEntryKind::End {
                reason,
                total_usage,
                turns: result.turns as u32,
            });
        }

        // Put surface back on self (we took it for borrow-checker
        // reasons). Idempotent — if caller drops the agent, this is
        // cleaned up normally.
        self.surface = surface;

        Ok(result)
    }
}

/// Read one user-turn: through the installed ConversationSurface
/// if any, else rustyline (sync stdin).
///
/// When a surface is installed, calls its `next_user_input` +
/// `on_turn_start(request_id)` to deliver the turn and reset
/// per-turn state (transcript buffer + streaming file for
/// ChatSurfaceState; no-op for TerminalSurface).
async fn read_next_user_turn(
    surface: &mut Option<Box<dyn super::surface::ConversationSurface>>,
    editor: &mut rustyline::DefaultEditor,
) -> Option<String> {
    use rustyline::error::ReadlineError;

    if let Some(s) = surface.as_mut() {
        let turn = s.next_user_input().await?;
        s.on_turn_start(turn.request_id.as_deref());
        return Some(turn.text);
    }
    loop {
        match editor.readline("\x1b[1;36m>\x1b[0m ") {
            Ok(line) => {
                // Blank line between user input and assistant reply
                // so turn boundaries are easy to scan.
                //
                // An earlier version tried to repaint the user's
                // line in light-yellow via cursor-up + clear + rewrite.
                // That worked on some terminals but miscounted rows on
                // others — responses drifted to column ~80 when the
                // input wrapped or when \x1b[1A didn't land where we
                // assumed. Doing it right needs rustyline's
                // Highlighter trait (live tinting as the user types,
                // no cursor math); until that refactor lands, drop
                // the post-hoc repaint and keep the separator only.
                eprintln!();
                return Some(line);
            }
            Err(ReadlineError::Interrupted) => {
                eprintln!(
                    "\x1b[2m(Ctrl-C — press again or /quit to exit, empty line to continue)\x1b[0m"
                );
                continue;
            }
            Err(ReadlineError::Eof) => return None,
            Err(e) => {
                eprintln!("\x1b[31m[nex] readline error: {}\x1b[0m", e);
                return None;
            }
        }
    }
}

fn truncate_for_display(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        // Walk back to the nearest char boundary so we don't slice a
        // multi-byte UTF-8 sequence. `floor_char_boundary` is nightly,
        // so do it by hand.
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Build a one-line summary of a tool output for the default
/// (non-chatty) display mode. Goals:
/// - short enough to fit on one terminal line
/// - informative enough that the human doesn't need to go look at the
///   session log for most routine calls
/// - degrades gracefully for empty / single-line / multi-line outputs
///
/// For single-line outputs ≤ 120 bytes, echoes the whole thing. For
/// multi-line or longer outputs, shows the first non-empty line
/// (truncated) plus a `(N lines, M bytes)` suffix.
fn summarize_tool_output(content: &str) -> String {
    let total_bytes = content.len();
    if total_bytes == 0 {
        return "ok (empty)".to_string();
    }
    let first_line = content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let total_lines = content.lines().count();

    const LINE_CAP: usize = 120;
    let truncated_first = if first_line.len() > LINE_CAP {
        let end = {
            let mut e = LINE_CAP;
            while e > 0 && !first_line.is_char_boundary(e) {
                e -= 1;
            }
            e
        };
        format!("{}…", &first_line[..end])
    } else {
        first_line.to_string()
    };

    if total_lines <= 1 && total_bytes <= LINE_CAP {
        if truncated_first.is_empty() {
            format!("ok ({} bytes)", total_bytes)
        } else {
            truncated_first
        }
    } else {
        format!(
            "{} ({} lines, {} bytes)",
            truncated_first, total_lines, total_bytes
        )
    }
}

/// Render a tool output to stderr under the per-call trace line with
/// uniform indent and sensible bounds — caps at both a byte limit and
/// a line-count limit so a multi-megabyte file read doesn't saturate
/// the terminal. `prefix` is printed before every output line (use for
/// indent + ANSI color), `suffix` after (use to close the color span).
fn print_indented_output(content: &str, prefix: &str, suffix: &str) {
    const MAX_LINES: usize = 20;
    const MAX_BYTES: usize = 1600;

    let truncated = truncate_for_display(content, MAX_BYTES);
    let mut line_count = 0usize;
    for line in truncated.lines() {
        if line_count >= MAX_LINES {
            break;
        }
        eprintln!("{}{}{}", prefix, line, suffix);
        line_count += 1;
    }

    let total_lines = content.lines().count();
    let total_bytes = content.len();
    let shown_lines = line_count;
    let shown_bytes = truncated.len();
    let line_overflow = total_lines > shown_lines;
    let byte_overflow = total_bytes > shown_bytes;
    if line_overflow || byte_overflow {
        let extra_lines = total_lines.saturating_sub(shown_lines);
        let extra_bytes = total_bytes.saturating_sub(shown_bytes);
        eprintln!(
            "{}… (+{} lines, +{} bytes truncated){}",
            prefix, extra_lines, extra_bytes, suffix
        );
    }
}

/// Result of processing a nex REPL slash command.
enum NexSlashResult {
    /// Command handled; the REPL should continue (prompt for next input).
    Continue,
    /// User asked to quit; the REPL should exit cleanly.
    Quit,
    /// Input was not actually a slash command (or was an unknown one
    /// treated as literal text); the caller should push it as a regular
    /// user message and proceed with an LLM turn.
    NotASlashCommand,
}

impl AgentLoop {
    /// Handle a nex REPL slash command. Returns what the caller should do
    /// next. Unknown `/...` inputs print a help hint and return `Continue`
    /// (they are NOT forwarded to the model as text).
    ///
    /// Supported commands:
    /// - `/help`, `/?`             — print the command list
    /// - `/quit`, `/exit`          — exit the REPL
    /// - `/clear`                  — clear conversation history
    /// - `/tokens`                 — print current cumulative token usage
    /// - `/save <path>`            — save session messages as JSON
    /// - `/load <path>`            — load session messages from JSON
    /// - `/bg run <cmd>`           — start a background task
    /// - `/bg list`                — list all background jobs
    /// - `/bg status <id>`         — show status of a specific job
    /// - `/bg output <id> [lines]` — stream output from a job
    /// - `/bg kill <id>`           — terminate a background job
    /// - `/bg delete <id>`         — remove a terminated job from the registry
    /// - `/cancel <id>`            — alias for `/bg kill <id>`
    async fn handle_nex_slash_command(
        &self,
        input: &str,
        messages: &mut Vec<Message>,
        total_usage: &Usage,
        turns: usize,
        compaction_count: u32,
        total_tokens_compacted: usize,
    ) -> NexSlashResult {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return NexSlashResult::NotASlashCommand;
        }
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").trim();

        match cmd {
            "/quit" | "/exit" => NexSlashResult::Quit,

            "/fork" => {
                // Fork the current session into a new one with a
                // copy of its journal. The fork evolves
                // independently from here — new inbox, new outbox,
                // writes don't affect the parent.
                let Some(ref wg_dir) = self.workgraph_dir else {
                    eprintln!("\x1b[33m[nex] /fork unavailable: no workgraph dir in scope\x1b[0m");
                    return NexSlashResult::Continue;
                };
                // Source session = the currently-bound chat surface.
                // Interactive sessions without a chat surface can't
                // be forked from inside /fork (they don't know their
                // own session_ref); users can still fork from another
                // terminal via `wg session fork <source>`.
                //
                // We read `chat_session_ref` (stored at with_chat_ref
                // time) rather than the surface itself — `run_interactive`
                // takes the surface out of self into a local, so by the
                // time a slash command runs, self.surface is None.
                let Some(source_ref) = self.chat_session_ref.clone() else {
                    eprintln!(
                        "\x1b[33m[nex] /fork: this session has no chat-surface ref; fork from another terminal with `wg session fork <source>`\x1b[0m"
                    );
                    return NexSlashResult::Continue;
                };
                let new_alias = if arg.is_empty() {
                    None
                } else {
                    Some(arg.to_string())
                };
                match crate::chat_sessions::fork_session(wg_dir, &source_ref, new_alias) {
                    Ok(fork_uuid) => {
                        let reg = crate::chat_sessions::load(wg_dir).ok();
                        let handle = reg
                            .as_ref()
                            .and_then(|r| r.sessions.get(&fork_uuid))
                            .and_then(|m| m.aliases.first().cloned())
                            .unwrap_or_else(|| fork_uuid.clone());
                        let short = &fork_uuid[..std::cmp::min(fork_uuid.len(), 8)];
                        eprintln!("\x1b[1;32m[nex]\x1b[0m forked → {} ({})", handle, short);
                        eprintln!("\x1b[2m  /quit and run: \x1b[0mwg nex --chat {}", handle);
                    }
                    Err(e) => eprintln!("\x1b[31m[nex] /fork failed: {}\x1b[0m", e),
                }
                NexSlashResult::Continue
            }

            "/sessions" | "/resume" => {
                // List all registered chat sessions, most-recent
                // first, with the exec command to resume each. We
                // don't switch mid-session (would require tearing
                // down + rebuilding the AgentLoop with a different
                // journal + message vec, which is a bigger refactor);
                // this is the "find the thing you want" half. The
                // user /quits and re-runs with the printed command.
                let Some(ref wg_dir) = self.workgraph_dir else {
                    eprintln!(
                        "\x1b[33m[nex] /{} unavailable: no workgraph dir in scope for this session\x1b[0m",
                        cmd.trim_start_matches('/')
                    );
                    return NexSlashResult::Continue;
                };
                match crate::chat_sessions::list(wg_dir) {
                    Ok(mut sessions) => {
                        // Sort by journal mtime (most-recent first).
                        sessions.sort_by(|a, b| {
                            let a_mt = std::fs::metadata(
                                crate::chat_sessions::chat_dir_for_uuid(wg_dir, &a.0)
                                    .join("conversation.jsonl"),
                            )
                            .and_then(|m| m.modified())
                            .ok();
                            let b_mt = std::fs::metadata(
                                crate::chat_sessions::chat_dir_for_uuid(wg_dir, &b.0)
                                    .join("conversation.jsonl"),
                            )
                            .and_then(|m| m.modified())
                            .ok();
                            b_mt.cmp(&a_mt).then_with(|| b.1.created.cmp(&a.1.created))
                        });
                        if sessions.is_empty() {
                            eprintln!("\x1b[2m[nex] no sessions registered yet\x1b[0m");
                        } else {
                            eprintln!(
                                "\x1b[1m{} session(s) (most recent first):\x1b[0m",
                                sessions.len()
                            );
                            for (uuid, meta) in sessions.iter().take(30) {
                                let short = &uuid[..std::cmp::min(uuid.len(), 8)];
                                let kind = format!("{:?}", meta.kind).to_lowercase();
                                let aliases = if meta.aliases.is_empty() {
                                    String::new()
                                } else {
                                    format!(" [{}]", meta.aliases.join(", "))
                                };
                                let handle = meta
                                    .aliases
                                    .first()
                                    .cloned()
                                    .unwrap_or_else(|| uuid.clone());
                                eprintln!(
                                    "  \x1b[1;36m{}\x1b[0m {}{}\n    \x1b[2m→ /quit and run: \x1b[0mwg nex --chat {}",
                                    short, kind, aliases, handle
                                );
                            }
                            if sessions.len() > 30 {
                                eprintln!(
                                    "\x1b[2m  ... {} more (use `wg session list` for the full set)\x1b[0m",
                                    sessions.len() - 30
                                );
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "\x1b[31m[nex] /{}: {}\x1b[0m",
                            cmd.trim_start_matches('/'),
                            e
                        );
                    }
                }
                NexSlashResult::Continue
            }

            "/help" | "/?" => {
                eprintln!(
                    "\x1b[1mNex REPL commands:\x1b[0m\n\
                     \x1b[1;36m  /help, /?\x1b[0m                    — this message\n\
                     \x1b[1;36m  /quit, /exit\x1b[0m                 — exit the REPL (Ctrl-D also works)\n\
                     \x1b[1;36m  /clear\x1b[0m                       — clear conversation history\n\
                     \x1b[1;36m  /tokens\x1b[0m                      — show cumulative token usage\n\
                     \x1b[1;36m  /save <path>\x1b[0m                 — save session to JSON file\n\
                     \x1b[1;36m  /load <path>\x1b[0m                 — load session from JSON file\n\
                     \x1b[1;36m  /bg run <cmd>\x1b[0m                — start a background task\n\
                     \x1b[1;36m  /bg list\x1b[0m                     — list all background jobs\n\
                     \x1b[1;36m  /bg status <id>\x1b[0m              — show status of a job\n\
                     \x1b[1;36m  /bg output <id> [lines]\x1b[0m      — stream output of a job\n\
                     \x1b[1;36m  /bg kill <id>\x1b[0m                — kill a background job\n\
                     \x1b[1;36m  /bg delete <id>\x1b[0m              — remove terminated job from registry\n\
                     \x1b[1;36m  /cancel <id>\x1b[0m                 — alias for /bg kill <id>\n\
                     \x1b[1;36m  /compact\x1b[0m                      — manually compact context (hard L2)\n\
                     \x1b[1;36m  /status\x1b[0m                       — show agent state (context, tokens, paths)\n\
                     \x1b[1;36m  /resume, /sessions\x1b[0m            — list all chat sessions with resume hints\n\
                     \x1b[1;36m  /fork [alias]\x1b[0m                 — fork this session (copy journal) to explore a different branch\n\
                     \n\
                     \x1b[2mCtrl-C during generation cancels the in-flight response.\x1b[0m\n\
                     \x1b[2mCtrl-C at the prompt is a no-op (use /quit or Ctrl-D to exit).\x1b[0m"
                );
                NexSlashResult::Continue
            }

            "/clear" => {
                messages.clear();
                eprintln!("\x1b[2m[nex] conversation cleared.\x1b[0m");
                NexSlashResult::Continue
            }

            "/tokens" => {
                eprintln!(
                    "\x1b[2m[nex] session: {} input + {} output tokens \
                     (= {} total, {} messages in history)\x1b[0m",
                    total_usage.input_tokens,
                    total_usage.output_tokens,
                    total_usage.input_tokens + total_usage.output_tokens,
                    messages.len(),
                );
                NexSlashResult::Continue
            }

            "/save" => {
                if arg.is_empty() {
                    eprintln!("\x1b[31m[nex] /save requires a path argument\x1b[0m");
                    return NexSlashResult::Continue;
                }
                match serde_json::to_string_pretty(messages) {
                    Ok(json) => match std::fs::write(arg, json) {
                        Ok(()) => eprintln!(
                            "\x1b[2m[nex] saved {} messages to {}\x1b[0m",
                            messages.len(),
                            arg
                        ),
                        Err(e) => eprintln!("\x1b[31m[nex] failed to write {}: {}\x1b[0m", arg, e),
                    },
                    Err(e) => eprintln!("\x1b[31m[nex] failed to serialize session: {}\x1b[0m", e),
                }
                NexSlashResult::Continue
            }

            "/load" => {
                if arg.is_empty() {
                    eprintln!("\x1b[31m[nex] /load requires a path argument\x1b[0m");
                    return NexSlashResult::Continue;
                }
                match std::fs::read_to_string(arg) {
                    Ok(text) => match serde_json::from_str::<Vec<Message>>(&text) {
                        Ok(loaded) => {
                            let n = loaded.len();
                            *messages = loaded;
                            eprintln!("\x1b[2m[nex] loaded {} messages from {}\x1b[0m", n, arg);
                        }
                        Err(e) => {
                            eprintln!("\x1b[31m[nex] failed to parse {}: {}\x1b[0m", arg, e)
                        }
                    },
                    Err(e) => eprintln!("\x1b[31m[nex] failed to read {}: {}\x1b[0m", arg, e),
                }
                NexSlashResult::Continue
            }

            "/bg" => {
                // Parse the bg subcommand.
                let mut bg_parts = arg.splitn(2, char::is_whitespace);
                let action = bg_parts.next().unwrap_or("");
                let rest = bg_parts.next().unwrap_or("").trim();
                if action.is_empty() {
                    eprintln!(
                        "\x1b[31m[nex] /bg requires an action: run|list|status|output|kill|delete\x1b[0m"
                    );
                    return NexSlashResult::Continue;
                }
                let input_json = match action {
                    "run" => {
                        if rest.is_empty() {
                            eprintln!("\x1b[31m[nex] /bg run requires a command\x1b[0m");
                            return NexSlashResult::Continue;
                        }
                        serde_json::json!({
                            "action": "run",
                            "command": rest,
                        })
                    }
                    "list" => serde_json::json!({ "action": "list" }),
                    "status" => {
                        if rest.is_empty() {
                            eprintln!("\x1b[31m[nex] /bg status requires a job id/name\x1b[0m");
                            return NexSlashResult::Continue;
                        }
                        serde_json::json!({ "action": "status", "job": rest })
                    }
                    "kill" => {
                        if rest.is_empty() {
                            eprintln!("\x1b[31m[nex] /bg kill requires a job id/name\x1b[0m");
                            return NexSlashResult::Continue;
                        }
                        serde_json::json!({ "action": "kill", "job": rest })
                    }
                    "output" => {
                        let mut out_parts = rest.splitn(2, char::is_whitespace);
                        let job = out_parts.next().unwrap_or("").trim();
                        if job.is_empty() {
                            eprintln!("\x1b[31m[nex] /bg output requires a job id/name\x1b[0m");
                            return NexSlashResult::Continue;
                        }
                        let lines: Option<i64> =
                            out_parts.next().and_then(|s| s.trim().parse().ok());
                        if let Some(n) = lines {
                            serde_json::json!({
                                "action": "output",
                                "job": job,
                                "lines": n,
                            })
                        } else {
                            serde_json::json!({ "action": "output", "job": job })
                        }
                    }
                    "delete" => {
                        if rest.is_empty() {
                            eprintln!("\x1b[31m[nex] /bg delete requires a job id/name\x1b[0m");
                            return NexSlashResult::Continue;
                        }
                        serde_json::json!({ "action": "delete", "job": rest })
                    }
                    other => {
                        eprintln!(
                            "\x1b[31m[nex] /bg: unknown action '{}'\x1b[0m — use run|list|status|output|kill|delete",
                            other
                        );
                        return NexSlashResult::Continue;
                    }
                };

                // Call the bg tool directly through the registry. No LLM
                // round-trip — the REPL gets immediate feedback.
                let result = self.tools.execute("bg", &input_json).await;
                if result.is_error {
                    eprintln!("\x1b[31m[nex] /bg error: {}\x1b[0m", result.content);
                } else {
                    // Print the tool result indented so it's visible as
                    // output, not conversation.
                    for line in result.content.lines() {
                        eprintln!("\x1b[2m  {}\x1b[0m", line);
                    }
                }
                NexSlashResult::Continue
            }

            "/cancel" => {
                if arg.is_empty() {
                    eprintln!("\x1b[31m[nex] /cancel requires a job id/name\x1b[0m");
                    return NexSlashResult::Continue;
                }
                let input_json = serde_json::json!({ "action": "kill", "job": arg });
                let result = self.tools.execute("bg", &input_json).await;
                if result.is_error {
                    eprintln!("\x1b[31m[nex] /cancel error: {}\x1b[0m", result.content);
                } else {
                    eprintln!("\x1b[2m[nex] {}\x1b[0m", result.content.trim());
                }
                NexSlashResult::Continue
            }

            "/compact" => {
                let pre_tokens = self.context_budget.effective_tokens(messages);
                let pre_count = messages.len();
                *messages = ContextBudget::hard_emergency_compact(messages.clone(), 1);
                let post_tokens = self.context_budget.effective_tokens(messages);
                let post_count = messages.len();
                eprintln!(
                    "\x1b[2m[nex] manual compaction: ~{} → ~{} tokens, {} → {} messages\x1b[0m",
                    pre_tokens, post_tokens, pre_count, post_count
                );
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: format!(
                            "[System note: user triggered manual compaction. ~{} → ~{} tokens, \
                             {} → {} messages. Earlier turns were summarized.]",
                            pre_tokens, post_tokens, pre_count, post_count
                        ),
                    }],
                });
                NexSlashResult::Continue
            }

            "/status" => {
                let token_est = self.context_budget.effective_tokens(messages);
                let ctx_window = self.client.context_window();
                let max_tok = self.client.max_tokens();
                let overhead = self.context_budget.overhead_tokens;
                let pct = if ctx_window > 0 {
                    (token_est as f64 / ctx_window as f64 * 100.0) as u32
                } else {
                    0
                };
                let user_msgs = messages.iter().filter(|m| m.role == Role::User).count();
                let asst_msgs = messages
                    .iter()
                    .filter(|m| m.role == Role::Assistant)
                    .count();
                let tool_results = messages
                    .iter()
                    .flat_map(|m| &m.content)
                    .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
                    .count();
                let available = ctx_window
                    .saturating_sub(overhead)
                    .saturating_sub(token_est);

                eprintln!("\x1b[1m── Agent Status ──\x1b[0m");
                eprintln!("  Model:         {}", self.client.model());
                eprintln!(
                    "  Context:       ~{} / {} tokens ({}%)",
                    token_est, ctx_window, pct
                );
                eprintln!(
                    "  Overhead:      ~{} tokens (system + tools + max_tokens={})",
                    overhead, max_tok
                );
                eprintln!("  Available:     ~{} tokens remaining", available);
                eprintln!(
                    "  Messages:      {} total ({} user, {} assistant, {} tool results)",
                    messages.len(),
                    user_msgs,
                    asst_msgs,
                    tool_results
                );
                eprintln!("  Turns:         {}", turns);
                eprintln!(
                    "  Tokens used:   {} input + {} output = {} total",
                    total_usage.input_tokens,
                    total_usage.output_tokens,
                    total_usage.input_tokens + total_usage.output_tokens
                );
                eprintln!(
                    "  Compactions:   {} fired, ~{} tokens freed total",
                    compaction_count, total_tokens_compacted
                );
                eprintln!("  Session log:   {}", self.output_log.display());
                if let Some(ref jp) = self.journal_path {
                    eprintln!("  Journal:       {}", jp.display());
                }
                eprintln!(
                    "  Tools:         {} registered",
                    self.tools.definitions().len()
                );
                NexSlashResult::Continue
            }

            other => {
                eprintln!(
                    "\x1b[31m[nex] unknown command: {}\x1b[0m — type /help for the list",
                    other
                );
                NexSlashResult::Continue
            }
        }
    }
}

/// Check whether an error is a request timeout.
///
/// RAII wrapper around a spinner thread. On drop, flips the stop
/// flag so the spinner erases its row and exits — guarantees the
/// spinner never outlives the LLM call, regardless of which path
/// the call unwinds through (success, retry storm, cancel, panic).
struct SpinnerGuard {
    stop: Arc<std::sync::atomic::AtomicBool>,
    _handle: std::thread::JoinHandle<()>,
}

impl SpinnerGuard {
    fn spawn(stop: Arc<std::sync::atomic::AtomicBool>) -> Self {
        let handle = start_spinner(stop.clone());
        SpinnerGuard {
            stop,
            _handle: handle,
        }
    }
}

impl Drop for SpinnerGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // Give the thread ~1 frame to observe the flag and erase
        // its row before further stderr writes land. 20ms is plenty
        // (spinner polls every 80ms but the stop check happens at
        // the top of each loop iter). We don't join because the
        // agent loop is on a tokio runtime — blocking joins would
        // stall the executor.
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

/// Spawn a thread that animates rainbow lightning-bolt spinner
/// rows on stderr until `stop` flips to true. On stop, the thread
/// erases its own row and exits. Mirrors the TUI's wave animation
/// (↯ bolts, Red/Orange/Green/Cyan/Violet palette, bright→dim
/// traveling peak) plus a dim elapsed-seconds counter so the user
/// can see whether the call is hanging vs just slow.
fn start_spinner(stop: Arc<std::sync::atomic::AtomicBool>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::Write;
        // Same palette as `tui::viz_viewer::render::spinner_wave_line`.
        const BRIGHT: [u8; 5] = [196, 214, 46, 33, 129];
        const MID: [u8; 5] = [124, 172, 34, 25, 91];
        const DIM: [u8; 5] = [52, 94, 22, 17, 53];
        let n = 5usize;
        let mut tick: usize = 0;
        let mut printed_anything = false;
        let started = std::time::Instant::now();
        // Small startup delay — if the model responds instantly we
        // don't want to flash a spinner for one frame.
        std::thread::sleep(std::time::Duration::from_millis(120));
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            let peak = tick % (n * 2);
            let peak = if peak >= n { n * 2 - 1 - peak } else { peak };
            let mut line = String::from("\r\x1b[2K");
            for i in 0..n {
                let d = (i as isize - peak as isize).unsigned_abs();
                let ansi = match d {
                    0 => format!("\x1b[1;38;5;{}m", BRIGHT[i]),
                    1 => format!("\x1b[38;5;{}m", BRIGHT[i]),
                    2 => format!("\x1b[38;5;{}m", MID[i]),
                    _ => format!("\x1b[38;5;{}m", DIM[i]),
                };
                line.push_str(&ansi);
                line.push('↯');
                line.push_str("\x1b[0m");
            }
            // Pad with spaces so residue from wider prior frames is
            // overwritten (there shouldn't be any — we always emit
            // the same width — but cheap insurance).
            // Dim elapsed-seconds counter — matches the TUI's style
            // so a user who's familiar with the TUI status bar knows
            // exactly what the number means.
            let elapsed = started.elapsed().as_secs();
            line.push_str(&format!(" \x1b[2;38;5;244m{}s\x1b[0m", elapsed));
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "{}", line);
            let _ = err.flush();
            drop(err);
            printed_anything = true;
            tick += 1;
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        if printed_anything {
            // Clear our row before exit.
            let mut err = std::io::stderr().lock();
            let _ = write!(err, "\r\x1b[2K");
            let _ = err.flush();
        }
    })
}

/// True when stderr is attached to an interactive terminal. One
/// syscall per call but only fires at turn boundaries, not per chunk.
fn stderr_is_tty() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::isatty(libc::STDERR_FILENO) != 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Best-effort terminal width via `TIOCGWINSZ`. Falls back to 80.
fn stderr_cols() -> usize {
    #[cfg(unix)]
    {
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                return ws.ws_col as usize;
            }
        }
    }
    80
}

/// Erase the just-streamed plain text on stderr and re-emit it as
/// rendered markdown. Counts newlines as a proxy for rows consumed —
/// if the stream had lines wider than the terminal those wrapped
/// and we'll miss the wrapped rows; the residue looks like a stale
/// fragment before the rendered version. Acceptable tradeoff for
/// the common "coordinator reply is short markdown" case.
fn rerender_markdown_on_stderr(buffer: &str) {
    use std::io::Write;
    let width = stderr_cols().max(20);
    let bytes = build_rerender_bytes(buffer, width);
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(bytes.as_bytes());
    let _ = err.flush();
}

/// Pure: compute the exact byte sequence to emit when re-rendering
/// `buffer` as markdown at terminal width `term_cols`.
///
/// The sequence is:
///   1. Cursor return + clear current row (for the trailing
///      blank row that the EndTurn arm's `eprintln!()` left us on).
///   2. `(rows_consumed - 0)` more "move up one row + clear row"
///      pairs, one per terminal row the streamed plain text took
///      up — wrapped logical lines are counted as multiple rows.
///   3. The rendered markdown as ANSI.
///
/// Counting terminal rows (not `\n` count) is the fix for a bug
/// where long lines soft-wrapped into multiple rows and the erase
/// undercounted — leftover wrapped tails of the plain stream then
/// appeared above the rendered version, producing visual doubling.
pub(crate) fn build_rerender_bytes(buffer: &str, term_cols: usize) -> String {
    let rendered = crate::markdown::markdown_to_ansi(buffer, term_cols);
    let stream_rows = rows_consumed(buffer, term_cols);
    // Plus 1: cursor is one row below the streamed content thanks
    // to the trailing `eprintln!()` in the EndTurn arm.
    let rows_to_clear = stream_rows + 1;
    let mut out = String::with_capacity(rendered.len() + rows_to_clear * 8);
    out.push_str("\r\x1b[2K");
    for _ in 1..rows_to_clear {
        out.push_str("\x1b[1A\x1b[2K");
    }
    out.push_str(&rendered);
    out
}

/// Count terminal rows occupied by `buffer` at column width
/// `term_cols`, accounting for `\n` and soft-wraps. `""` → 0.
pub(crate) fn rows_consumed(buffer: &str, term_cols: usize) -> usize {
    if buffer.is_empty() {
        return 0;
    }
    let w = term_cols.max(1);
    let mut rows = 1usize;
    let mut col = 0usize;
    for ch in buffer.chars() {
        if ch == '\n' {
            rows += 1;
            col = 0;
            continue;
        }
        let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if ch_w == 0 {
            // Zero-width (combining, ZWJ). Doesn't advance column.
            continue;
        }
        if col + ch_w > w {
            rows += 1;
            col = ch_w;
        } else {
            col += ch_w;
        }
    }
    rows
}

/// Detects both reqwest timeouts and generic timeout messages from the error chain.
fn is_timeout_error(err: &anyhow::Error) -> bool {
    // Check reqwest-specific timeout
    if let Some(req_err) = err.downcast_ref::<reqwest::Error>()
        && req_err.is_timeout()
    {
        return true;
    }
    // Check error message chain for timeout indicators
    let msg = format!("{:#}", err).to_lowercase();
    msg.contains("timed out") || msg.contains("timeout") || msg.contains("deadline exceeded")
}

#[cfg(test)]
mod rerender_tests {
    use super::*;

    // ── rows_consumed ──

    #[test]
    fn rows_empty_buffer_is_zero() {
        assert_eq!(rows_consumed("", 80), 0);
    }

    #[test]
    fn rows_short_line_is_one() {
        assert_eq!(rows_consumed("hello", 80), 1);
    }

    #[test]
    fn rows_trailing_newline_adds_empty_row() {
        // "hello\n" leaves the cursor on an empty row below.
        assert_eq!(rows_consumed("hello\n", 80), 2);
    }

    #[test]
    fn rows_multiple_lines_each_counted() {
        assert_eq!(rows_consumed("a\nb\nc", 80), 3);
    }

    #[test]
    fn rows_soft_wrap_counts_each_terminal_row() {
        // 10-char line, width 5 → wraps to 2 rows.
        assert_eq!(rows_consumed("abcdefghij", 5), 2);
        // 12-char line, width 5 → wraps to 3 rows (5+5+2).
        assert_eq!(rows_consumed("abcdefghijkl", 5), 3);
        // 5-char line exactly fits width → 1 row.
        assert_eq!(rows_consumed("abcde", 5), 1);
    }

    #[test]
    fn rows_mixed_wrap_and_newlines() {
        // Two logical lines: "ab" (1 row) and "cccccccccc" at w=4 →
        // ceil(10/4) = 3 rows. Total 4.
        assert_eq!(rows_consumed("ab\ncccccccccc", 4), 4);
    }

    // ── build_rerender_bytes ──

    fn count_occurrences(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    #[test]
    fn rerender_erases_then_renders_short_line() {
        let out = build_rerender_bytes("hello", 80);
        // stream_rows=1 → rows_to_clear=2 → one "clear current" + one
        // "up + clear" pair. Move-up escape shows up once.
        assert_eq!(count_occurrences(&out, "\x1b[1A"), 1);
        // Plus the rendered text at the end.
        assert!(out.contains("hello"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn rerender_wide_line_erases_all_wrap_rows_not_just_the_logical_line() {
        // 60 chars at width 20 → 3 terminal rows of content + 1 trailing
        // empty row → 4 rows_to_clear → 3 "up + clear" escapes.
        let long = "x".repeat(60);
        let out = build_rerender_bytes(&long, 20);
        assert_eq!(
            count_occurrences(&out, "\x1b[1A"),
            3,
            "must erase every wrapped row to avoid the stream-tail duplication bug"
        );
    }

    #[test]
    fn rerender_emits_rendered_markdown_with_ansi_styling() {
        let out = build_rerender_bytes("# Heading\n", 80);
        // Heading should carry ANSI color codes.
        assert!(out.contains("Heading"));
        assert!(out.contains("\x1b["));
    }

    #[test]
    fn rerender_bullet_glyph_substituted() {
        let out = build_rerender_bytes("- one\n- two\n", 80);
        assert!(out.contains('•'));
        assert!(out.contains("one"));
        assert!(out.contains("two"));
    }

    #[test]
    fn rerender_inline_code_has_background() {
        let out = build_rerender_bytes("See `foo` here.\n", 80);
        assert!(out.contains("foo"));
        // Background-set escape (`48;5;` → indexed background).
        assert!(out.contains("\x1b[48;5;"));
    }

    // ── SpinnerGuard drop behavior ──

    #[test]
    fn spinner_guard_stops_thread_on_drop() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = Arc::new(AtomicBool::new(false));
        {
            let _g = SpinnerGuard::spawn(stop.clone());
            // Let the spinner thread run for a few frames.
            std::thread::sleep(std::time::Duration::from_millis(200));
            // flag should still be false — thread is looping.
            assert!(!stop.load(Ordering::SeqCst));
        }
        // Drop fires here — guard waits briefly for the thread to
        // observe the stop. After that, flag must be true.
        assert!(
            stop.load(Ordering::SeqCst),
            "SpinnerGuard::drop should flip stop flag so the thread exits"
        );
    }

    #[test]
    fn spinner_guard_drop_is_idempotent_via_clone_semantics() {
        use std::sync::atomic::{AtomicBool, Ordering};
        // Two guards on the same atomic can both fire Drop; the
        // flag just stays true. Sanity check for the Arc-shared
        // stop flag pattern.
        let stop = Arc::new(AtomicBool::new(false));
        drop(SpinnerGuard::spawn(stop.clone()));
        assert!(stop.load(Ordering::SeqCst));
        drop(SpinnerGuard::spawn(stop.clone()));
        assert!(stop.load(Ordering::SeqCst));
    }
}
