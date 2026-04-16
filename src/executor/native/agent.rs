//! Tool-use loop for the native executor.
//!
//! Manages the conversation lifecycle: sends messages to the API, executes
//! tool calls, and loops until the agent produces a final text response or
//! hits the max-turns limit.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
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
            ContextBudget::with_window_size(client.context_window()).with_overhead(overhead)
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
        use rustyline::error::ReadlineError;

        let mut messages: Vec<Message> = Vec::new();
        let mut total_usage = Usage::default();
        let mut tool_calls = Vec::new();
        let mut turns: usize = 0;
        let mut consecutive_server_errors: u32 = 0;
        const MAX_CONSECUTIVE_SERVER_ERRORS: u32 = 3;
        // Compaction escalation state — same pattern as `AgentLoop::run`.
        // Drives the three-tier ladder: L1 soft → L2 hard → L3 summarize.
        let mut nex_noop_streak: u32 = 0;
        let mut nex_l3_fired: bool = false;
        // Compaction tracking for /status display.
        let mut compaction_count: u32 = 0;
        let mut total_tokens_compacted: usize = 0;
        // Why the REPL is exiting — recorded in the SessionEnd log event.
        // The initial value is a defensive fallback; in practice every
        // `break` from the main loop overwrites it before the
        // `log_session_end` call at the end.
        #[allow(unused_assignments)]
        let mut session_exit_reason: &'static str = "eof";

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

        // Helper: read a user line with rustyline. Returns:
        // - `Some(line)` on normal input (empty line allowed — caller filters)
        // - `None` on Ctrl-D (EOF) or non-recoverable error → exit REPL
        // - Loops on Ctrl-C at the prompt (does NOT exit — just re-prompts)
        let read_user_input = |editor: &mut DefaultEditor| -> Option<String> {
            loop {
                match editor.readline("\x1b[1;36m>\x1b[0m ") {
                    Ok(line) => return Some(line),
                    Err(ReadlineError::Interrupted) => {
                        // Ctrl-C at the prompt: re-display and re-read.
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
        };

        // If resume already populated messages (session summary or journal
        // replay), skip the first-input readline — we go straight to the
        // main loop.
        let resumed = session_summary.is_some() || resume_data.is_some();
        if !resumed {
            // Fresh start — get the first user message
            let first_input = if let Some(msg) = initial_message {
                msg.to_string()
            } else {
                match read_user_input(&mut editor) {
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
            if turns >= self.max_turns {
                eprintln!(
                    "\n\x1b[33m[nex] Max turns ({}) reached.\x1b[0m",
                    self.max_turns
                );
                session_exit_reason = "max_turns";
                break;
            }

            // If the last entry isn't a user message (e.g., we just
            // handled a slash command that printed info), prompt again
            // before the next LLM call.
            let needs_user_input = messages
                .last()
                .map(|m| m.role != Role::User)
                .unwrap_or(true);
            if needs_user_input {
                match read_user_input(&mut editor) {
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
            let streaming_file = self.streaming_file_path.clone();
            let stream_writer_clone = self.stream_writer.clone();
            let is_autonomous = self.autonomous;
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
                    eprint!("{}", text);
                    let _ = std::io::stderr().flush();
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
                            nex_noop_streak = 0;

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
                // Interactive mode: Ctrl-C cancels the in-flight call
                let streaming_future = self.client.send_streaming(&request, &on_text);
                let ctrl_c_future = tokio::signal::ctrl_c();
                tokio::select! {
                    biased;
                    _ = ctrl_c_future => {
                        eprintln!(
                            "\n\x1b[33m[nex] Interrupted — dropping in-flight response.\x1b[0m"
                        );
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
                                nex_noop_streak = 0;
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

            messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            // Journal the assistant message so the full conversation
            // is replayable from the journal file.
            if let Some(ref mut j) = journal {
                let _ = j.append(JournalEntryKind::Message {
                    role: Role::Assistant,
                    content: response.content.clone(),
                    usage: Some(response.usage.clone()),
                    response_id: Some(response.id.clone()),
                    stop_reason: response.stop_reason,
                });
            }

            match response.stop_reason {
                Some(StopReason::EndTurn) | Some(StopReason::StopSequence) | None => {
                    if !self.autonomous {
                        let has_text = response
                            .content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::Text { text } if !text.is_empty()));
                        if has_text {
                            eprintln!();
                        }
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

                    // Add a blank line between the assistant's response
                    // and our next prompt. The readline call handles
                    // rustyline's own display.
                    eprintln!();
                    match read_user_input(&mut editor) {
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

                    for (i, (id, name, input)) in tool_use_blocks.iter().enumerate() {
                        if input.get("__parse_error").is_some() {
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

                        // Streaming callback factory for bash tool output
                        let streaming_file = self.streaming_file_path.clone();
                        let stream_writer_clone = self.stream_writer.clone();
                        let make_callback = move |_idx: usize| {
                            let sw = stream_writer_clone.clone();
                            let sf = streaming_file.clone();
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
                        // Interactive mode: Ctrl-C aware tool execution.
                        let batch_future = self
                            .tools
                            .execute_batch(&calls_only, super::tools::DEFAULT_MAX_CONCURRENT_TOOLS);
                        let ctrl_c_future = tokio::signal::ctrl_c();
                        tokio::select! {
                            biased;
                            _ = ctrl_c_future => {
                                eprintln!(
                                    "\n\x1b[33m[nex] Interrupted during tool execution — returning to prompt.\x1b[0m"
                                );
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
                    // Three-tier escalation ladder: L1 soft → L2 hard
                    // → L3 summarize-history.
                    let pre_tokens = self.context_budget.effective_tokens(&messages);
                    let pre_count = messages.len();

                    let streak = nex_noop_streak;
                    const ESCALATION_THRESHOLD: u32 = 2;
                    let use_l3 = streak >= ESCALATION_THRESHOLD * 2 && !nex_l3_fired;
                    let use_hard = !use_l3 && streak >= ESCALATION_THRESHOLD;

                    let tier_name = if use_l3 {
                        "L3 summarize-history"
                    } else if use_hard {
                        "L2 hard"
                    } else {
                        "L1 soft"
                    };

                    messages = if use_l3 {
                        nex_l3_fired = true;
                        super::tools::summarize::summarize_history_for_compaction(
                            self.client.as_ref(),
                            messages,
                        )
                        .await
                    } else if use_hard {
                        ContextBudget::hard_emergency_compact(messages, 1)
                    } else {
                        ContextBudget::emergency_compact(messages, 2)
                    };

                    let post_tokens = self.context_budget.effective_tokens(&messages);
                    let post_count = messages.len();
                    let delta = pre_tokens.saturating_sub(post_tokens);

                    if delta > 0 || use_hard || use_l3 {
                        nex_noop_streak = 0;
                    } else {
                        nex_noop_streak = nex_noop_streak.saturating_add(1);
                    }

                    compaction_count += 1;
                    total_tokens_compacted += delta;

                    if self.autonomous {
                        eprintln!(
                            "[native-agent] {} compaction: ~{} → ~{} tokens (Δ -{}, {} → {} messages, overhead {} kept, noop_streak={})",
                            tier_name,
                            pre_tokens,
                            post_tokens,
                            delta,
                            pre_count,
                            post_count,
                            self.context_budget.overhead_tokens,
                            nex_noop_streak,
                        );
                    } else if self.nex_verbose {
                        eprintln!(
                            "\x1b[33m[nex] {} compaction: ~{} → ~{} tokens (Δ -{}, {} → {} msgs, noop_streak={})\x1b[0m",
                            tier_name,
                            pre_tokens,
                            post_tokens,
                            delta,
                            pre_count,
                            post_count,
                            nex_noop_streak,
                        );
                    }

                    // Inject a context note so the model knows
                    // compaction happened and can adjust. Without
                    // this, the model has no signal that earlier
                    // context was compressed and may try to reference
                    // details that are no longer in its window.
                    if !self.autonomous {
                        eprintln!(
                            "\x1b[2m[context compacted: ~{} → ~{} tokens via {}]\x1b[0m",
                            pre_tokens, post_tokens, tier_name
                        );
                    }
                    messages.push(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: format!(
                                "[System note: context was compacted ({}) — earlier turns \
                                 were summarized. ~{} → ~{} tokens, {} → {} messages. \
                                 If you need details from earlier, check the session \
                                 log or ask the user to re-state.]",
                                tier_name, pre_tokens, post_tokens, pre_count, post_count
                            ),
                        }],
                    });

                    // Journal the compaction event
                    if let Some(ref mut j) = journal {
                        let compacted_through_seq = j.seq();
                        let _ = j.append(JournalEntryKind::Compaction {
                            compacted_through_seq,
                            summary: format!(
                                "{} compaction. Tokens: ~{} → ~{}, messages: {} → {}.",
                                tier_name, pre_tokens, post_tokens, pre_count, post_count
                            ),
                            original_message_count: pre_count as u32,
                            original_token_count: pre_tokens as u32,
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

        Ok(result)
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
