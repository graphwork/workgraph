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
use super::resume::{self, ContextBudget, ContextPressureAction, ResumeConfig};
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

        // Build context budget from the provider's context window
        let context_budget = ContextBudget::with_window_size(client.context_window());

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
        }
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

    /// Run the agent loop to completion.
    pub async fn run(&mut self, initial_message: &str) -> Result<AgentResult> {
        // Try to load a session summary for faster resume (replaces raw history)
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

        // Attempt resume from existing journal (only if no session summary available)
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

        // Open journal if configured (append mode — continues from existing entries)
        let mut journal = if let Some(ref path) = self.journal_path {
            match Journal::open(path) {
                Ok(j) => Some(j),
                Err(e) => {
                    eprintln!("[native-agent] Warning: failed to open journal: {}", e);
                    None
                }
            }
        } else {
            None
        };

        // Write Init journal entry for this session
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

        // Write Init stream event
        if let Some(ref sw) = self.stream_writer {
            sw.write_init("native", Some(self.client.model()), None);
        }

        // Build initial messages — from session summary, journal resume, or fresh start
        let mut messages: Vec<Message> = if let Some(ref summary) = session_summary {
            // Resume from session summary (compact representation of prior work)
            let resume_text = format!(
                "IMPORTANT: This task is being RESUMED from a prior agent session. \
                 Below is a summary of what was accomplished:\n\n{}\n\n---\n\n{}\n\n\
                 [Continue from where the previous agent left off. The summary above \
                 replaces the full conversation history for efficiency.]",
                summary, initial_message
            );

            let content = vec![ContentBlock::Text { text: resume_text }];

            // Journal the summary-based resume message
            if let Some(ref mut j) = journal {
                let _ = j.append(JournalEntryKind::Message {
                    role: Role::User,
                    content: content.clone(),
                    usage: None,
                    response_id: None,
                    stop_reason: None,
                });
            }

            vec![Message {
                role: Role::User,
                content,
            }]
        } else if let Some(ref data) = resume_data {
            // Start with the resumed conversation history
            let mut msgs = data.messages.clone();

            // Build and inject the resume annotation + fresh initial message
            let annotation = resume::build_resume_annotation(data);
            let resume_text = format!(
                "{}\n\n---\n\n{}\n\n[Continuing from prior session. Review the conversation above and pick up where you left off.]",
                annotation, initial_message
            );

            msgs.push(Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: resume_text }],
            });

            // Journal the resume user message
            if let Some(ref mut j) = journal {
                let _ = j.append(JournalEntryKind::Message {
                    role: Role::User,
                    content: msgs.last().unwrap().content.clone(),
                    usage: None,
                    response_id: None,
                    stop_reason: None,
                });
            }

            msgs
        } else {
            // Fresh start — no resume
            let initial_content = vec![ContentBlock::Text {
                text: initial_message.to_string(),
            }];

            // Journal the initial user message
            if let Some(ref mut j) = journal {
                let _ = j.append(JournalEntryKind::Message {
                    role: Role::User,
                    content: initial_content.clone(),
                    usage: None,
                    response_id: None,
                    stop_reason: None,
                });
            }

            vec![Message {
                role: Role::User,
                content: initial_content,
            }]
        };

        let mut total_usage = Usage::default();
        let mut tool_calls = Vec::new();
        let mut turns = 0;
        let mut consecutive_server_errors: u32 = 0;
        const MAX_CONSECUTIVE_SERVER_ERRORS: u32 = 3;

        loop {
            if turns >= self.max_turns {
                eprintln!(
                    "[native-agent] Max turns ({}) reached, stopping",
                    self.max_turns
                );
                break;
            }

            // ── Mid-turn state injection (ephemeral) ──────────────────────
            // Collect dynamic state changes and build request_messages with
            // any injections appended. These are NOT persisted to the journal
            // or to the `messages` vec — they appear once in the API request
            // and then vanish.
            let request_messages = if let Some(ref mut injector) = self.state_injector {
                // Get context pressure warning (if at warning threshold)
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
                    // Clone messages and append injection to the last user message
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

            // Build streaming callback that writes text chunks to stream.jsonl
            // and updates the .streaming file for TUI live display.
            let streaming_file = self.streaming_file_path.clone();
            let stream_writer_clone = self.stream_writer.clone();
            let on_text = move |text: String| {
                // Write TextChunk to stream.jsonl
                if let Some(ref sw) = stream_writer_clone {
                    sw.write_text_chunk(&text);
                }
                // Update .streaming file with accumulated text
                if let Some(ref path) = streaming_file {
                    let mut accumulated = std::fs::read_to_string(path).unwrap_or_default();
                    accumulated.push_str(&text);
                    let _ = std::fs::write(path, &accumulated);
                }
            };

            let response = match self.client.send_streaming(&request, &on_text).await {
                Ok(resp) => resp,
                Err(e) => {
                    // Check for context-too-long errors (400/413) — attempt emergency
                    // compaction and retry once before giving up.
                    if super::openai_client::is_context_too_long(&e) {
                        eprintln!(
                            "[native-agent] Context too long error — attempting emergency compaction and retry"
                        );
                        let pre_compact_len = messages.len();
                        messages = ContextBudget::emergency_compact(messages, 5);
                        eprintln!(
                            "[native-agent] Emergency compacted: {} → {} messages",
                            pre_compact_len,
                            messages.len()
                        );

                        // Rebuild request with compacted messages and retry once
                        let retry_request = MessagesRequest {
                            model: self.client.model().to_string(),
                            max_tokens: self.client.max_tokens(),
                            system: Some(self.system_prompt.clone()),
                            messages: messages.clone(),
                            tools: if self.supports_tools {
                                self.tools.definitions()
                            } else {
                                vec![]
                            },
                            stream: false,
                        };

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
                                // Log progress and return gracefully
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
                    } else if let Some(api_err) = e.downcast_ref::<super::openai_client::ApiError>()
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
                                // Gracefully surface to model — it never sees the raw error
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
                            return Err(e)
                                .context("Request timeout — consecutive failures exceeded limit");
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
            };

            // Successful response — reset consecutive error counter
            consecutive_server_errors = 0;

            // Clean up .streaming file after each turn
            if let Some(ref path) = self.streaming_file_path {
                let _ = std::fs::remove_file(path);
            }

            total_usage.add(&response.usage);
            turns += 1;

            // Log the assistant turn
            self.log_turn(turns, &response);

            // Session summary extraction: after every N turns, extract and store
            if self.summary_interval_turns > 0
                && turns % self.summary_interval_turns == 0
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

            // Journal the assistant message
            if let Some(ref mut j) = journal {
                let _ = j.append(JournalEntryKind::Message {
                    role: Role::Assistant,
                    content: response.content.clone(),
                    usage: Some(response.usage.clone()),
                    response_id: Some(response.id.clone()),
                    stop_reason: response.stop_reason,
                });
            }

            // Write Turn stream event
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

            // Add assistant response to conversation
            messages.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
            });

            match response.stop_reason {
                Some(StopReason::EndTurn) | Some(StopReason::StopSequence) => {
                    // Agent is done — extract final text
                    let final_text = response
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    let result = AgentResult {
                        final_text,
                        turns,
                        total_usage: total_usage.clone(),
                        tool_calls,
                    };
                    self.log_result(&result);
                    self.write_stream_result(true, &result);
                    self.store_final_summary(&messages);

                    // Journal End entry
                    if let Some(ref mut j) = journal {
                        let _ = j.append(JournalEntryKind::End {
                            reason: EndReason::Complete,
                            total_usage,
                            turns: result.turns as u32,
                        });
                    }

                    return Ok(result);
                }
                Some(StopReason::ToolUse) => {
                    // Collect tool_use blocks from the response
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

                    // Execute batch (read-only in parallel, mutating serially)
                    // For bash tool, use streaming execution to stream output to TUI.
                    let calls_only: Vec<_> =
                        batch_calls.iter().map(|(_, _, c)| c.clone()).collect();

                    // Create streaming callback factory for bash tool output
                    let streaming_file = self.streaming_file_path.clone();
                    let stream_writer_clone = self.stream_writer.clone();

                    let make_callback = move |_idx: usize| {
                        let sw = stream_writer_clone.clone();
                        let sf = streaming_file.clone();
                        Box::new(move |text: String| {
                            // Write ToolOutputChunk to stream.jsonl
                            if let Some(ref writer) = sw {
                                writer.write_tool_output_chunk("bash", &text);
                            }
                            // Append to .streaming file for TUI live display
                            if let Some(ref path) = sf {
                                let mut acc = std::fs::read_to_string(path).unwrap_or_default();
                                acc.push_str(&text);
                                acc.push('\n');
                                let _ = std::fs::write(path, &acc);
                            }
                        }) as super::tools::ToolStreamCallback
                    };

                    let batch_results = self
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

                    // Merge parse-error results and batch results into original order
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

                        // Log the tool call
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

                        tool_calls.push(ToolCallRecord {
                            name: name.clone(),
                            input: input.clone(),
                            output: output.content.clone(),
                            is_error: output.is_error,
                        });

                        results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: output.content.clone(),
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
                    // Response truncated — prompt for continuation
                    let continuation = vec![ContentBlock::Text {
                        text: "Your response was truncated. Please continue.".to_string(),
                    }];

                    // Journal the continuation message
                    if let Some(ref mut j) = journal {
                        let _ = j.append(JournalEntryKind::Message {
                            role: Role::User,
                            content: continuation.clone(),
                            usage: None,
                            response_id: None,
                            stop_reason: None,
                        });
                    }

                    messages.push(Message {
                        role: Role::User,
                        content: continuation,
                    });
                }
                None => {
                    // No stop reason — treat as end
                    let final_text = response
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");

                    let result = AgentResult {
                        final_text,
                        turns,
                        total_usage: total_usage.clone(),
                        tool_calls,
                    };
                    self.log_result(&result);
                    self.write_stream_result(true, &result);
                    self.store_final_summary(&messages);

                    // Journal End entry
                    if let Some(ref mut j) = journal {
                        let _ = j.append(JournalEntryKind::End {
                            reason: EndReason::Complete,
                            total_usage,
                            turns: result.turns as u32,
                        });
                    }

                    return Ok(result);
                }
            }

            // ── Context pressure check ──────────────────────────────────
            // After processing the turn, check if we're approaching context limits.
            // When state injection is active, warnings are handled ephemerally in the
            // pre-turn injection (not permanently appended to messages).
            match self.context_budget.check_pressure(&messages) {
                ContextPressureAction::Ok => {}
                ContextPressureAction::Warning => {
                    if self.state_injector.is_some() {
                        // With state injection: warning is handled ephemerally pre-turn.
                        // Just log it here — the actual injection happens above.
                        eprintln!(
                            "[native-agent] Context pressure at warning level — will inject ephemerally next turn"
                        );
                    } else {
                        // Legacy path: append to messages (non-ephemeral fallback).
                        let warning = self.context_budget.warning_message(&messages);
                        eprintln!("[native-agent] {}", warning);
                        if let Some(last) = messages.last_mut()
                            && last.role == Role::User
                        {
                            last.content.push(ContentBlock::Text { text: warning });
                        }
                    }
                }
                ContextPressureAction::EmergencyCompaction => {
                    // 90%+ capacity: emergency compaction — strip old tool results
                    let pre_compact = messages.len();
                    messages = ContextBudget::emergency_compact(messages, 5);
                    eprintln!(
                        "[native-agent] Emergency compaction at 90%: {} → {} messages",
                        pre_compact,
                        messages.len()
                    );

                    // Journal the compaction event
                    if let Some(ref mut j) = journal {
                        let _ = j.append(JournalEntryKind::Compaction {
                            compacted_through_seq: 0,
                            summary: format!(
                                "Emergency compaction triggered at ~90% context capacity. {} messages compacted.",
                                pre_compact
                            ),
                            original_message_count: pre_compact as u32,
                            original_token_count: 0,
                        });
                    }
                }
                ContextPressureAction::CleanExit => {
                    // 95%+ capacity: clean exit — log progress and stop gracefully
                    eprintln!("[native-agent] Context at 95%+ capacity — performing clean exit");
                    self.store_final_summary(&messages);

                    let final_text = "[context limit reached — clean exit]".to_string();
                    let result = AgentResult {
                        final_text,
                        turns,
                        total_usage: total_usage.clone(),
                        tool_calls,
                    };
                    self.log_result(&result);
                    self.write_stream_result(false, &result);

                    if let Some(ref mut j) = journal {
                        let _ = j.append(JournalEntryKind::End {
                            reason: EndReason::MaxTurns,
                            total_usage,
                            turns: result.turns as u32,
                        });
                    }

                    return Ok(result);
                }
            }
        }

        // Max turns reached — store final summary before returning
        self.store_final_summary(&messages);

        let result = AgentResult {
            final_text: "[max turns reached]".to_string(),
            turns,
            total_usage: total_usage.clone(),
            tool_calls,
        };
        self.log_result(&result);
        self.write_stream_result(false, &result);

        // Journal End entry for max turns
        if let Some(ref mut j) = journal {
            let _ = j.append(JournalEntryKind::End {
                reason: EndReason::MaxTurns,
                total_usage,
                turns: result.turns as u32,
            });
        }

        Ok(result)
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
            // Also write to stdout so the wrapper script captures it to output.log,
            // making events visible to the TUI.
            println!("{}", json);
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
