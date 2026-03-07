//! Unified stream event format for all executor types.
//!
//! All executors produce NDJSON events to `<agent_dir>/stream.jsonl`.
//! The coordinator reads these files for liveness detection, cost tracking,
//! and progress monitoring.

use std::io::Write;
use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Unified stream event emitted by all executor types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// First event — session/run metadata.
    Init {
        executor_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        timestamp_ms: i64,
    },
    /// Agent completed one turn of the tool-use loop.
    Turn {
        turn_number: u32,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tools_used: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<TurnUsage>,
        timestamp_ms: i64,
    },
    /// Tool execution started.
    ToolStart {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        timestamp_ms: i64,
    },
    /// Tool execution completed.
    ToolEnd {
        name: String,
        is_error: bool,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output_summary: Option<String>,
        timestamp_ms: i64,
    },
    /// Periodic heartbeat.
    Heartbeat { timestamp_ms: i64 },
    /// Final event — aggregated usage and outcome.
    Result {
        success: bool,
        usage: TotalUsage,
        timestamp_ms: i64,
    },
}

/// Token usage for a single turn.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
}

/// Aggregated token usage for an entire run.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TotalUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl StreamEvent {
    /// Get the timestamp_ms from any event variant.
    pub fn timestamp_ms(&self) -> i64 {
        match self {
            StreamEvent::Init { timestamp_ms, .. }
            | StreamEvent::Turn { timestamp_ms, .. }
            | StreamEvent::ToolStart { timestamp_ms, .. }
            | StreamEvent::ToolEnd { timestamp_ms, .. }
            | StreamEvent::Heartbeat { timestamp_ms }
            | StreamEvent::Result { timestamp_ms, .. } => *timestamp_ms,
        }
    }
}

/// Current millisecond timestamp.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ── Tool detail extraction (single source of truth) ─────────────────────

/// Extract a human-readable detail string from a tool invocation.
///
/// This is the canonical function for tool detail display across all executors.
/// Given a tool name and its JSON input, returns a concise summary like
/// `"Bash: cargo test"` or `"Read: src/main.rs"`.
pub fn extract_tool_detail(name: &str, input: &serde_json::Value) -> Option<String> {
    let detail = match name {
        "Bash" => input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|c| {
                let c = c.trim();
                if c.len() > 80 {
                    format!("{name}: {}…", &c[..c.floor_char_boundary(80)])
                } else {
                    format!("{name}: {c}")
                }
            }),
        "Read" | "Write" | "Edit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .map(|p| format!("{name}: {p}")),
        "Grep" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|p| format!("{name}: {p}")),
        "Glob" => input
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|p| format!("{name}: {p}")),
        _ => None,
    };
    detail
}

/// Summarize tool output for display in ToolEnd events.
///
/// Produces a truncated, single-line summary of the tool's output text.
/// Returns `None` if the output is empty.
pub fn summarize_tool_output(output: &str, max_len: usize) -> Option<String> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Take the last non-empty line (most relevant for status output)
    let line = trimmed
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(trimmed)
        .trim();
    if line.len() <= max_len {
        Some(line.to_string())
    } else {
        // Reserve space for the "…" suffix (3 bytes in UTF-8)
        let boundary = line.floor_char_boundary(max_len.saturating_sub(3));
        Some(format!("{}…", &line[..boundary]))
    }
}

// ── Token usage from stream events ──────────────────────────────────────

/// Compute token usage from a stream.jsonl file (canonical token source).
///
/// Reads the stream file and computes accumulated usage from Turn events,
/// or uses the Result event's total if present. This replaces parsing the
/// raw output.log for token data.
pub fn parse_token_usage_from_stream(agent_dir: &Path) -> Option<crate::graph::TokenUsage> {
    let stream_path = agent_dir.join(STREAM_FILE_NAME);
    let raw_path = agent_dir.join(RAW_STREAM_FILE_NAME);

    let events = if stream_path.exists() {
        read_stream_events(&stream_path, 0).ok().map(|(e, _)| e)
    } else if raw_path.exists() {
        translate_claude_stream(&raw_path, 0).ok().map(|(e, _)| e)
    } else {
        None
    }?;

    if events.is_empty() {
        return None;
    }

    let mut state = AgentStreamState::default();
    let fake_offset = 0; // offset doesn't matter for token computation
    state.ingest(&events, fake_offset);
    Some(state.to_token_usage())
}

// ── NDJSON file reading ─────────────────────────────────────────────────

/// Read stream events from an NDJSON file, starting at `offset` bytes.
///
/// Returns the parsed events and the new file offset for incremental reads.
/// Lines that fail to parse are silently skipped (partial writes, etc.).
pub fn read_stream_events(path: &Path, offset: u64) -> Result<(Vec<StreamEvent>, u64)> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let end = file.metadata()?.len();

    if offset >= end {
        return Ok((Vec::new(), offset));
    }

    file.seek(SeekFrom::Start(offset))?;
    let reader = BufReader::new(&file);

    let mut events = Vec::new();
    let mut new_offset = offset;

    for line in reader.lines() {
        let line = line?;
        new_offset += line.len() as u64 + 1; // +1 for newline
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<StreamEvent>(line) {
            events.push(event);
        }
    }

    Ok((events, new_offset))
}

// ── NDJSON file writing ─────────────────────────────────────────────────

/// Writer that appends StreamEvent records as NDJSON to a file.
pub struct StreamWriter {
    path: std::path::PathBuf,
}

impl StreamWriter {
    /// Create a new stream writer for the given file path.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Write a single event to the stream file.
    pub fn write_event(&self, event: &StreamEvent) {
        if let Ok(json) = serde_json::to_string(event)
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
        {
            let _ = writeln!(file, "{}", json);
        }
    }

    /// Write the Init event.
    pub fn write_init(&self, executor_type: &str, model: Option<&str>, session_id: Option<&str>) {
        self.write_event(&StreamEvent::Init {
            executor_type: executor_type.to_string(),
            model: model.map(String::from),
            session_id: session_id.map(String::from),
            timestamp_ms: now_ms(),
        });
    }

    /// Write a Turn event.
    pub fn write_turn(&self, turn_number: u32, tools_used: Vec<String>, usage: Option<TurnUsage>) {
        self.write_event(&StreamEvent::Turn {
            turn_number,
            tools_used,
            usage,
            timestamp_ms: now_ms(),
        });
    }

    /// Write a ToolStart event.
    pub fn write_tool_start(&self, name: &str, detail: Option<String>) {
        self.write_event(&StreamEvent::ToolStart {
            name: name.to_string(),
            detail,
            timestamp_ms: now_ms(),
        });
    }

    /// Write a ToolEnd event.
    pub fn write_tool_end(
        &self,
        name: &str,
        is_error: bool,
        duration_ms: u64,
        output_summary: Option<String>,
    ) {
        self.write_event(&StreamEvent::ToolEnd {
            name: name.to_string(),
            is_error,
            duration_ms,
            output_summary,
            timestamp_ms: now_ms(),
        });
    }

    /// Write a Heartbeat event.
    pub fn write_heartbeat(&self) {
        self.write_event(&StreamEvent::Heartbeat {
            timestamp_ms: now_ms(),
        });
    }

    /// Write the final Result event.
    pub fn write_result(&self, success: bool, usage: TotalUsage) {
        self.write_event(&StreamEvent::Result {
            success,
            usage,
            timestamp_ms: now_ms(),
        });
    }

    /// Get the path to the stream file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ── Claude CLI JSONL translation ────────────────────────────────────────

/// Translate a Claude CLI raw JSONL line into a StreamEvent.
///
/// Claude CLI emits events like `{"type":"assistant","message":{...,"usage":{...}}}`,
/// `{"type":"result","total_cost_usd":...,"usage":{...}}`, etc.
/// We translate the ones we care about into our unified format.
pub fn translate_claude_event(line: &str) -> Option<StreamEvent> {
    let val: serde_json::Value = serde_json::from_str(line).ok()?;
    let event_type = val.get("type")?.as_str()?;

    match event_type {
        "system" => {
            // Init-like event — extract session info
            let session_id = val
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let model = val.get("model").and_then(|v| v.as_str()).map(String::from);
            Some(StreamEvent::Init {
                executor_type: "claude".to_string(),
                model,
                session_id,
                timestamp_ms: now_ms(),
            })
        }
        "assistant" => {
            // Turn completed — extract usage from message.usage
            let usage = val.get("message").and_then(|m| m.get("usage"));
            let turn_usage = usage.map(|u| TurnUsage {
                input_tokens: u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                output_tokens: u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
                cache_read_input_tokens: u
                    .get("cache_read_input_tokens")
                    .or_else(|| u.get("cacheReadInputTokens"))
                    .and_then(|v| v.as_u64()),
                cache_creation_input_tokens: u
                    .get("cache_creation_input_tokens")
                    .or_else(|| u.get("cacheCreationInputTokens"))
                    .and_then(|v| v.as_u64()),
            });

            // Extract tool names from content blocks
            let tools_used: Vec<String> = val
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
                        .filter_map(|b| b.get("name").and_then(|n| n.as_str()).map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            Some(StreamEvent::Turn {
                turn_number: 0, // Claude CLI doesn't number turns
                tools_used,
                usage: turn_usage,
                timestamp_ms: now_ms(),
            })
        }
        "result" => {
            // Final result — extract total usage and cost
            let usage = val.get("usage");
            let cost = val.get("total_cost_usd").and_then(|v| v.as_f64());

            let total_usage = TotalUsage {
                input_tokens: usage
                    .and_then(|u| u.get("input_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output_tokens: usage
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_read_input_tokens: usage
                    .and_then(|u| {
                        u.get("cache_read_input_tokens")
                            .or_else(|| u.get("cacheReadInputTokens"))
                    })
                    .and_then(|v| v.as_u64()),
                cache_creation_input_tokens: usage
                    .and_then(|u| {
                        u.get("cache_creation_input_tokens")
                            .or_else(|| u.get("cacheCreationInputTokens"))
                    })
                    .and_then(|v| v.as_u64()),
                cost_usd: cost,
                model: None,
            };

            Some(StreamEvent::Result {
                success: true,
                usage: total_usage,
                timestamp_ms: now_ms(),
            })
        }
        _ => None,
    }
}

/// Translate a file of raw Claude CLI JSONL into StreamEvents.
///
/// Reads `raw_stream.jsonl` from `offset`, translates each line, and returns
/// the StreamEvents plus the new offset.
pub fn translate_claude_stream(path: &Path, offset: u64) -> Result<(Vec<StreamEvent>, u64)> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let end = file.metadata()?.len();

    if offset >= end {
        return Ok((Vec::new(), offset));
    }

    file.seek(SeekFrom::Start(offset))?;
    let reader = BufReader::new(&file);

    let mut events = Vec::new();
    let mut new_offset = offset;

    for line in reader.lines() {
        let line = line?;
        new_offset += line.len() as u64 + 1;
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        if let Some(event) = translate_claude_event(line) {
            events.push(event);
        }
    }

    Ok((events, new_offset))
}

// ── Liveness detection ──────────────────────────────────────────────────

/// Stream state tracked per agent by the coordinator.
#[derive(Debug, Clone, Default)]
pub struct AgentStreamState {
    /// Byte offset into the stream file (for incremental reads).
    pub offset: u64,
    /// Timestamp (ms) of the last event seen.
    pub last_event_ms: Option<i64>,
    /// Number of turns observed.
    pub turn_count: u32,
    /// Current tool being executed (if any).
    pub current_tool: Option<String>,
    /// Accumulated usage across all turns.
    pub accumulated_usage: TotalUsage,
}

impl AgentStreamState {
    /// Update state with a batch of new events.
    pub fn ingest(&mut self, events: &[StreamEvent], new_offset: u64) {
        for event in events {
            self.last_event_ms = Some(event.timestamp_ms());

            match event {
                StreamEvent::Init { model, .. } => {
                    self.accumulated_usage.model = model.clone();
                }
                StreamEvent::Turn {
                    turn_number, usage, ..
                } => {
                    self.turn_count = *turn_number;
                    self.current_tool = None;
                    if let Some(u) = usage {
                        self.accumulated_usage.input_tokens += u.input_tokens;
                        self.accumulated_usage.output_tokens += u.output_tokens;
                        if let Some(cr) = u.cache_read_input_tokens {
                            *self
                                .accumulated_usage
                                .cache_read_input_tokens
                                .get_or_insert(0) += cr;
                        }
                        if let Some(cc) = u.cache_creation_input_tokens {
                            *self
                                .accumulated_usage
                                .cache_creation_input_tokens
                                .get_or_insert(0) += cc;
                        }
                    }
                }
                StreamEvent::ToolStart { name, .. } => {
                    self.current_tool = Some(name.clone());
                }
                StreamEvent::ToolEnd { .. } => {
                    self.current_tool = None;
                }
                StreamEvent::Heartbeat { .. } => {}
                StreamEvent::Result { usage, .. } => {
                    // Final usage overwrites accumulated
                    self.accumulated_usage = usage.clone();
                }
            }
        }
        self.offset = new_offset;
    }

    /// Returns true if the stream is stale (no events for the given duration).
    pub fn is_stale(&self, stale_threshold_ms: i64) -> bool {
        match self.last_event_ms {
            Some(last) => now_ms() - last > stale_threshold_ms,
            None => false, // No events yet — not stale, just not started
        }
    }

    /// Convert accumulated usage to a `TokenUsage` for storage in the graph.
    pub fn to_token_usage(&self) -> crate::graph::TokenUsage {
        crate::graph::TokenUsage {
            cost_usd: self.accumulated_usage.cost_usd.unwrap_or(0.0),
            input_tokens: self.accumulated_usage.input_tokens,
            output_tokens: self.accumulated_usage.output_tokens,
            cache_read_input_tokens: self.accumulated_usage.cache_read_input_tokens.unwrap_or(0),
            cache_creation_input_tokens: self
                .accumulated_usage
                .cache_creation_input_tokens
                .unwrap_or(0),
        }
    }
}

/// The standard stream file name within an agent's output directory.
pub const STREAM_FILE_NAME: &str = "stream.jsonl";

/// The raw Claude CLI output file (before translation).
pub const RAW_STREAM_FILE_NAME: &str = "raw_stream.jsonl";

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_stream_event_roundtrip() {
        let events = vec![
            StreamEvent::Init {
                executor_type: "claude".to_string(),
                model: Some("claude-sonnet-4-20250514".to_string()),
                session_id: Some("sess-123".to_string()),
                timestamp_ms: 1000,
            },
            StreamEvent::Turn {
                turn_number: 1,
                tools_used: vec!["Bash".to_string(), "Read".to_string()],
                usage: Some(TurnUsage {
                    input_tokens: 500,
                    output_tokens: 200,
                    cache_read_input_tokens: Some(100),
                    cache_creation_input_tokens: None,
                }),
                timestamp_ms: 2000,
            },
            StreamEvent::ToolStart {
                name: "Bash".to_string(),
                detail: Some("Bash: cargo test".to_string()),
                timestamp_ms: 3000,
            },
            StreamEvent::ToolEnd {
                name: "Bash".to_string(),
                is_error: false,
                duration_ms: 150,
                output_summary: Some("test result: ok".to_string()),
                timestamp_ms: 3150,
            },
            StreamEvent::Heartbeat { timestamp_ms: 4000 },
            StreamEvent::Result {
                success: true,
                usage: TotalUsage {
                    input_tokens: 1000,
                    output_tokens: 500,
                    cache_read_input_tokens: Some(200),
                    cache_creation_input_tokens: Some(50),
                    cost_usd: Some(0.05),
                    model: Some("claude-sonnet-4-20250514".to_string()),
                },
                timestamp_ms: 5000,
            },
        ];

        for event in &events {
            let json = serde_json::to_string(event).unwrap();
            let parsed: StreamEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(&parsed, event);
        }
    }

    #[test]
    fn test_write_and_read_stream() {
        let dir = TempDir::new().unwrap();
        let stream_path = dir.path().join("stream.jsonl");

        let writer = StreamWriter::new(&stream_path);
        writer.write_init("native", Some("gpt-4"), None);
        writer.write_turn(
            1,
            vec!["Bash".to_string()],
            Some(TurnUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            }),
        );
        writer.write_heartbeat();
        writer.write_result(
            true,
            TotalUsage {
                input_tokens: 100,
                output_tokens: 50,
                cost_usd: Some(0.01),
                ..Default::default()
            },
        );

        let (events, offset) = read_stream_events(&stream_path, 0).unwrap();
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], StreamEvent::Init { .. }));
        assert!(matches!(events[1], StreamEvent::Turn { .. }));
        assert!(matches!(events[2], StreamEvent::Heartbeat { .. }));
        assert!(matches!(events[3], StreamEvent::Result { .. }));
        assert!(offset > 0);

        // Incremental read from offset should yield nothing new
        let (events2, offset2) = read_stream_events(&stream_path, offset).unwrap();
        assert!(events2.is_empty());
        assert_eq!(offset2, offset);
    }

    #[test]
    fn test_translate_claude_assistant_event() {
        let line = r#"{"type":"assistant","message":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":500,"output_tokens":100,"cache_read_input_tokens":50}}}"#;
        let event = translate_claude_event(line).unwrap();
        match event {
            StreamEvent::Turn {
                tools_used, usage, ..
            } => {
                assert_eq!(tools_used, vec!["Bash"]);
                let u = usage.unwrap();
                assert_eq!(u.input_tokens, 500);
                assert_eq!(u.output_tokens, 100);
                assert_eq!(u.cache_read_input_tokens, Some(50));
            }
            _ => panic!("Expected Turn event"),
        }
    }

    #[test]
    fn test_translate_claude_result_event() {
        let line = r#"{"type":"result","total_cost_usd":0.123,"usage":{"input_tokens":1000,"output_tokens":500,"cache_read_input_tokens":200}}"#;
        let event = translate_claude_event(line).unwrap();
        match event {
            StreamEvent::Result { success, usage, .. } => {
                assert!(success);
                assert_eq!(usage.input_tokens, 1000);
                assert_eq!(usage.output_tokens, 500);
                assert_eq!(usage.cost_usd, Some(0.123));
            }
            _ => panic!("Expected Result event"),
        }
    }

    #[test]
    fn test_translate_claude_system_event() {
        let line = r#"{"type":"system","session_id":"abc123","model":"claude-sonnet-4-20250514"}"#;
        let event = translate_claude_event(line).unwrap();
        match event {
            StreamEvent::Init {
                executor_type,
                model,
                session_id,
                ..
            } => {
                assert_eq!(executor_type, "claude");
                assert_eq!(model.as_deref(), Some("claude-sonnet-4-20250514"));
                assert_eq!(session_id.as_deref(), Some("abc123"));
            }
            _ => panic!("Expected Init event"),
        }
    }

    #[test]
    fn test_translate_unknown_event_returns_none() {
        let line = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"hello"}}"#;
        assert!(translate_claude_event(line).is_none());
    }

    #[test]
    fn test_agent_stream_state_ingest() {
        let events = vec![
            StreamEvent::Init {
                executor_type: "claude".to_string(),
                model: Some("sonnet".to_string()),
                session_id: None,
                timestamp_ms: 1000,
            },
            StreamEvent::Turn {
                turn_number: 1,
                tools_used: vec!["Bash".to_string()],
                usage: Some(TurnUsage {
                    input_tokens: 500,
                    output_tokens: 200,
                    cache_read_input_tokens: Some(100),
                    cache_creation_input_tokens: None,
                }),
                timestamp_ms: 2000,
            },
            StreamEvent::ToolStart {
                name: "Read".to_string(),
                detail: Some("Read: src/main.rs".to_string()),
                timestamp_ms: 3000,
            },
        ];

        let mut state = AgentStreamState::default();
        state.ingest(&events, 500);

        assert_eq!(state.last_event_ms, Some(3000));
        assert_eq!(state.turn_count, 1);
        assert_eq!(state.current_tool.as_deref(), Some("Read"));
        assert_eq!(state.accumulated_usage.input_tokens, 500);
        assert_eq!(state.accumulated_usage.output_tokens, 200);
        assert_eq!(state.accumulated_usage.model.as_deref(), Some("sonnet"));
        assert_eq!(state.offset, 500);
    }

    #[test]
    fn test_agent_stream_state_staleness() {
        let mut state = AgentStreamState::default();
        // No events yet → not stale
        assert!(!state.is_stale(5000));

        // Recent event → not stale
        state.last_event_ms = Some(now_ms());
        assert!(!state.is_stale(5000));

        // Old event → stale
        state.last_event_ms = Some(now_ms() - 10_000);
        assert!(state.is_stale(5000));
    }

    #[test]
    fn test_to_token_usage() {
        let state = AgentStreamState {
            accumulated_usage: TotalUsage {
                input_tokens: 1000,
                output_tokens: 500,
                cache_read_input_tokens: Some(200),
                cache_creation_input_tokens: Some(50),
                cost_usd: Some(0.05),
                model: Some("test".to_string()),
            },
            ..Default::default()
        };

        let token_usage = state.to_token_usage();
        assert_eq!(token_usage.input_tokens, 1000);
        assert_eq!(token_usage.output_tokens, 500);
        assert_eq!(token_usage.cache_read_input_tokens, 200);
        assert_eq!(token_usage.cache_creation_input_tokens, 50);
        assert_eq!(token_usage.cost_usd, 0.05);
    }

    #[test]
    fn test_read_stream_events_with_bad_lines() {
        let dir = TempDir::new().unwrap();
        let stream_path = dir.path().join("stream.jsonl");

        // Write a mix of valid and invalid lines
        let content = r#"{"type":"heartbeat","timestamp_ms":1000}
not json
{"type":"unknown_type","data":"foo"}
{"type":"heartbeat","timestamp_ms":2000}
"#;
        std::fs::write(&stream_path, content).unwrap();

        let (events, _) = read_stream_events(&stream_path, 0).unwrap();
        // Only the two heartbeats should parse — unknown_type doesn't match our enum
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn test_translate_claude_stream_file() {
        let dir = TempDir::new().unwrap();
        let raw_path = dir.path().join("raw_stream.jsonl");

        let content = r#"{"type":"system","session_id":"s1","model":"claude-sonnet-4-20250514"}
{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}],"usage":{"input_tokens":100,"output_tokens":50}}}
{"type":"result","total_cost_usd":0.01,"usage":{"input_tokens":100,"output_tokens":50}}
"#;
        std::fs::write(&raw_path, content).unwrap();

        let (events, offset) = translate_claude_stream(&raw_path, 0).unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], StreamEvent::Init { .. }));
        assert!(matches!(events[1], StreamEvent::Turn { .. }));
        assert!(matches!(events[2], StreamEvent::Result { .. }));
        assert!(offset > 0);
    }

    #[test]
    fn test_extract_tool_detail_bash() {
        let input = serde_json::json!({"command": "cargo test --lib"});
        assert_eq!(
            extract_tool_detail("Bash", &input),
            Some("Bash: cargo test --lib".to_string())
        );
    }

    #[test]
    fn test_extract_tool_detail_read() {
        let input = serde_json::json!({"file_path": "/src/main.rs"});
        assert_eq!(
            extract_tool_detail("Read", &input),
            Some("Read: /src/main.rs".to_string())
        );
    }

    #[test]
    fn test_extract_tool_detail_grep() {
        let input = serde_json::json!({"pattern": "fn main"});
        assert_eq!(
            extract_tool_detail("Grep", &input),
            Some("Grep: fn main".to_string())
        );
    }

    #[test]
    fn test_extract_tool_detail_unknown_tool() {
        let input = serde_json::json!({"foo": "bar"});
        assert_eq!(extract_tool_detail("CustomTool", &input), None);
    }

    #[test]
    fn test_extract_tool_detail_bash_long_command() {
        let long_cmd = "a".repeat(120);
        let input = serde_json::json!({"command": long_cmd});
        let result = extract_tool_detail("Bash", &input).unwrap();
        assert!(result.len() <= 90); // "Bash: " + 80 + "…"
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_summarize_tool_output_short() {
        assert_eq!(
            summarize_tool_output("hello world", 60),
            Some("hello world".to_string())
        );
    }

    #[test]
    fn test_summarize_tool_output_multiline() {
        let output = "line 1\nline 2\nline 3";
        assert_eq!(
            summarize_tool_output(output, 60),
            Some("line 3".to_string())
        );
    }

    #[test]
    fn test_summarize_tool_output_long() {
        let long = "x".repeat(100);
        let result = summarize_tool_output(&long, 60).unwrap();
        assert!(result.len() <= 60);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_summarize_tool_output_empty() {
        assert_eq!(summarize_tool_output("", 60), None);
        assert_eq!(summarize_tool_output("  \n  ", 60), None);
    }

    #[test]
    fn test_parse_token_usage_from_stream_with_turns() {
        let dir = TempDir::new().unwrap();
        let stream_path = dir.path().join(STREAM_FILE_NAME);
        let writer = StreamWriter::new(&stream_path);

        writer.write_init("test", None, None);
        writer.write_turn(
            1,
            vec!["Bash".to_string()],
            Some(TurnUsage {
                input_tokens: 500,
                output_tokens: 200,
                cache_read_input_tokens: Some(100),
                cache_creation_input_tokens: None,
            }),
        );
        writer.write_turn(
            2,
            vec!["Read".to_string()],
            Some(TurnUsage {
                input_tokens: 300,
                output_tokens: 150,
                cache_read_input_tokens: Some(50),
                cache_creation_input_tokens: None,
            }),
        );

        let usage = parse_token_usage_from_stream(dir.path()).unwrap();
        assert_eq!(usage.input_tokens, 800);
        assert_eq!(usage.output_tokens, 350);
        assert_eq!(usage.cache_read_input_tokens, 150);
    }

    #[test]
    fn test_parse_token_usage_from_stream_with_result() {
        let dir = TempDir::new().unwrap();
        let stream_path = dir.path().join(STREAM_FILE_NAME);
        let writer = StreamWriter::new(&stream_path);

        writer.write_init("test", None, None);
        writer.write_turn(
            1,
            vec![],
            Some(TurnUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            }),
        );
        writer.write_result(
            true,
            TotalUsage {
                input_tokens: 100,
                output_tokens: 50,
                cost_usd: Some(0.05),
                ..Default::default()
            },
        );

        let usage = parse_token_usage_from_stream(dir.path()).unwrap();
        assert_eq!(usage.input_tokens, 100);
        assert_eq!(usage.output_tokens, 50);
        assert_eq!(usage.cost_usd, 0.05);
    }

    #[test]
    fn test_parse_token_usage_from_stream_no_files() {
        let dir = TempDir::new().unwrap();
        assert!(parse_token_usage_from_stream(dir.path()).is_none());
    }

    #[test]
    fn test_stream_event_detail_serialization() {
        // ToolStart with detail should include it in JSON
        let event = StreamEvent::ToolStart {
            name: "Bash".to_string(),
            detail: Some("Bash: ls -la".to_string()),
            timestamp_ms: 1000,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("detail"));
        assert!(json.contains("Bash: ls -la"));

        // ToolStart without detail should omit the field
        let event_no_detail = StreamEvent::ToolStart {
            name: "Bash".to_string(),
            detail: None,
            timestamp_ms: 1000,
        };
        let json2 = serde_json::to_string(&event_no_detail).unwrap();
        assert!(!json2.contains("detail"));

        // Both should roundtrip
        let parsed: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
        let parsed2: StreamEvent = serde_json::from_str(&json2).unwrap();
        assert_eq!(parsed2, event_no_detail);
    }

    #[test]
    fn test_stream_event_output_summary_serialization() {
        let event = StreamEvent::ToolEnd {
            name: "Bash".to_string(),
            is_error: false,
            duration_ms: 100,
            output_summary: Some("test passed".to_string()),
            timestamp_ms: 2000,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("output_summary"));
        let parsed: StreamEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, event);
    }
}
