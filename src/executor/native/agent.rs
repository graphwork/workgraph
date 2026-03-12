//! Tool-use loop for the native executor.
//!
//! Manages the conversation lifecycle: sends messages to the API, executes
//! tool calls, and loops until the agent produces a final text response or
//! hits the max-turns limit.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Serialize;

use super::client::{
    ContentBlock, Message, MessagesRequest, MessagesResponse, Role, StopReason, Usage,
};
use super::provider::Provider;
use super::tools::ToolRegistry;
use crate::stream_event::{self, StreamWriter, TotalUsage, TurnUsage};

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
    ToolUse { id: String, name: String },
    ToolResult { tool_use_id: String, is_error: bool },
}

impl From<&ContentBlock> for ContentBlockLog {
    fn from(block: &ContentBlock) -> Self {
        match block {
            ContentBlock::Text { text } => ContentBlockLog::Text { text: text.clone() },
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

        Self {
            client,
            tools,
            system_prompt,
            max_turns,
            output_log,
            stream_writer,
            supports_tools,
        }
    }

    /// Run the agent loop to completion.
    pub async fn run(&self, initial_message: &str) -> Result<AgentResult> {
        // Write Init stream event
        if let Some(ref sw) = self.stream_writer {
            sw.write_init("native", Some(self.client.model()), None);
        }

        let mut messages: Vec<Message> = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: initial_message.to_string(),
            }],
        }];

        let mut total_usage = Usage::default();
        let mut tool_calls = Vec::new();
        let mut turns = 0;

        loop {
            if turns >= self.max_turns {
                eprintln!(
                    "[native-agent] Max turns ({}) reached, stopping",
                    self.max_turns
                );
                break;
            }

            let request = MessagesRequest {
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

            let response = self
                .client
                .send(&request)
                .await
                .context("API request failed")?;

            total_usage.add(&response.usage);
            turns += 1;

            // Log the assistant turn
            self.log_turn(turns, &response);

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
                        total_usage,
                        tool_calls,
                    };
                    self.log_result(&result);
                    self.write_stream_result(true, &result);
                    return Ok(result);
                }
                Some(StopReason::ToolUse) => {
                    // Execute all tool_use blocks and collect results
                    let mut results = Vec::new();
                    for block in &response.content {
                        if let ContentBlock::ToolUse { id, name, input } = block {
                            // Stream: tool start
                            if let Some(ref sw) = self.stream_writer {
                                sw.write_tool_start(name);
                            }
                            let tool_start = std::time::Instant::now();

                            let output = self.tools.execute(name, input).await;

                            // Stream: tool end
                            if let Some(ref sw) = self.stream_writer {
                                sw.write_tool_end(
                                    name,
                                    output.is_error,
                                    tool_start.elapsed().as_millis() as u64,
                                );
                            }

                            // Log the tool call
                            self.log_tool_call(name, input, &output.content, output.is_error);

                            tool_calls.push(ToolCallRecord {
                                name: name.clone(),
                                input: input.clone(),
                                output: output.content.clone(),
                                is_error: output.is_error,
                            });

                            results.push(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: output.content,
                                is_error: output.is_error,
                            });
                        }
                    }
                    messages.push(Message {
                        role: Role::User,
                        content: results,
                    });
                }
                Some(StopReason::MaxTokens) => {
                    // Response truncated — prompt for continuation
                    messages.push(Message {
                        role: Role::User,
                        content: vec![ContentBlock::Text {
                            text: "Your response was truncated. Please continue.".to_string(),
                        }],
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
                        total_usage,
                        tool_calls,
                    };
                    self.log_result(&result);
                    self.write_stream_result(true, &result);
                    return Ok(result);
                }
            }
        }

        // Max turns reached
        let result = AgentResult {
            final_text: "[max turns reached]".to_string(),
            turns,
            total_usage,
            tool_calls,
        };
        self.log_result(&result);
        self.write_stream_result(false, &result);
        Ok(result)
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
                    cost_usd: None, // Native executor doesn't track cost (no price table yet)
                    model: Some(self.client.model().to_string()),
                },
            );
        }
    }

    fn write_log_event(&self, event: &LogEvent) {
        if let Ok(json) = serde_json::to_string(event)
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.output_log)
        {
            let _ = writeln!(file, "{}", json);
        }
    }
}
