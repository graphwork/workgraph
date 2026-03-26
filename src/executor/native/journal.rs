//! Conversation journal for the native executor.
//!
//! Persists every message exchange to a structured, provider-agnostic JSONL file
//! at `.workgraph/output/<task-id>/conversation.jsonl`. Enables resume, debugging,
//! evaluation, and audit.
//!
//! See docs/research/unified-conversation-layer-design.md §3 for the specification.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use super::client::{ContentBlock, Role, StopReason, ToolDefinition, Usage};

/// A single entry in the conversation journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Monotonically increasing sequence number within this conversation.
    pub seq: u64,

    /// ISO-8601 timestamp of when this entry was recorded.
    pub timestamp: String,

    /// The kind of entry.
    #[serde(flatten)]
    pub kind: JournalEntryKind,
}

/// The kind of journal entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "entry_type", rename_all = "snake_case")]
pub enum JournalEntryKind {
    /// Conversation metadata — first entry in every journal.
    Init {
        model: String,
        provider: String,
        system_prompt: String,
        /// Tool definitions available in this conversation.
        tools: Vec<ToolDefinition>,
        /// Task ID if running within workgraph.
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
    },

    /// A message in the conversation (user or assistant).
    Message {
        role: Role,
        content: Vec<ContentBlock>,
        /// Usage stats (present only for assistant messages).
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// API response ID (present only for assistant messages).
        #[serde(skip_serializing_if = "Option::is_none")]
        response_id: Option<String>,
        /// Stop reason (present only for assistant messages).
        #[serde(skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
    },

    /// A tool execution record.
    ToolExecution {
        /// Matches the tool_use id in the preceding assistant message.
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
        output: String,
        is_error: bool,
        /// Wall-clock duration in milliseconds.
        duration_ms: u64,
    },

    /// Compaction marker — indicates that messages before this point were summarized.
    Compaction {
        /// Sequence number of the last entry that was compacted.
        compacted_through_seq: u64,
        /// The summary that replaces the compacted messages.
        summary: String,
        /// Number of original messages that were compacted.
        original_message_count: u32,
        /// Total tokens in the compacted region.
        original_token_count: u32,
    },

    /// Conversation ended.
    End {
        reason: EndReason,
        total_usage: Usage,
        turns: u32,
    },
}

/// Reason a conversation ended.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// Agent produced a final text response.
    Complete,
    /// Hit max turns limit.
    MaxTurns,
    /// Agent was interrupted/crashed (written on resume, not at crash time).
    Interrupted,
    /// Error during execution.
    Error { message: String },
}

/// Append-only conversation journal writer.
///
/// Each entry is flushed immediately to ensure crash safety — entries up to the
/// last flush are guaranteed on disk.
pub struct Journal {
    file: File,
    seq: u64,
}

impl Journal {
    /// Create or open (for append) a journal file.
    ///
    /// If the file already exists, reads it to determine the current sequence
    /// number so that new entries continue the sequence.
    pub fn open(path: &Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create journal directory: {}", parent.display())
            })?;
        }

        // Determine current seq by reading existing entries
        let seq = if path.exists() {
            // If the file doesn't end with a newline (crash mid-write), add one
            // so the next appended entry starts on its own line.
            if let Ok(data) = std::fs::read(path)
                && !data.is_empty()
                && data.last() != Some(&b'\n')
                && let Ok(mut f) = OpenOptions::new().append(true).open(path)
            {
                let _ = writeln!(f);
            }
            Self::last_seq(path)?
        } else {
            0
        };

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("Failed to open journal file: {}", path.display()))?;

        Ok(Self { file, seq })
    }

    /// Append an entry, auto-assigning seq and timestamp.
    pub fn append(&mut self, kind: JournalEntryKind) -> Result<()> {
        self.seq += 1;
        let entry = JournalEntry {
            seq: self.seq,
            timestamp: Utc::now().to_rfc3339(),
            kind,
        };

        let json = serde_json::to_string(&entry).context("Failed to serialize journal entry")?;
        writeln!(self.file, "{}", json).context("Failed to write journal entry")?;
        self.file.flush().context("Failed to flush journal entry")?;

        // Best-effort fsync for durability — don't fail the agent if it errors
        let _ = self.file.sync_data();

        Ok(())
    }

    /// Current sequence number.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Read all entries from a journal file.
    ///
    /// Malformed lines (e.g. from a crash mid-write) are skipped with a warning.
    pub fn read_all(path: &Path) -> Result<Vec<JournalEntry>> {
        let file = File::open(path)
            .with_context(|| format!("Failed to open journal for reading: {}", path.display()))?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();

        for (line_num, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("Failed to read line {}", line_num + 1))?;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<JournalEntry>(line) {
                Ok(entry) => entries.push(entry),
                Err(e) => {
                    eprintln!(
                        "[journal] Warning: skipping malformed entry at line {}: {}",
                        line_num + 1,
                        e
                    );
                }
            }
        }

        Ok(entries)
    }

    /// Determine the last sequence number in an existing journal file.
    fn last_seq(path: &Path) -> Result<u64> {
        let entries = Self::read_all(path)?;
        Ok(entries.last().map(|e| e.seq).unwrap_or(0))
    }
}

/// Construct the journal path for a task.
pub fn journal_path(workgraph_dir: &Path, task_id: &str) -> std::path::PathBuf {
    workgraph_dir
        .join("output")
        .join(task_id)
        .join("conversation.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_journal_create_and_append() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("conversation.jsonl");

        let mut journal = Journal::open(&path).unwrap();
        assert_eq!(journal.seq(), 0);

        journal
            .append(JournalEntryKind::Init {
                model: "test-model".to_string(),
                provider: "test-provider".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                task_id: Some("test-task".to_string()),
            })
            .unwrap();
        assert_eq!(journal.seq(), 1);

        journal
            .append(JournalEntryKind::Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Hello".to_string(),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
        assert_eq!(journal.seq(), 2);

        // Read back
        let entries = Journal::read_all(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 1);
        assert_eq!(entries[1].seq, 2);

        // Verify Init entry
        match &entries[0].kind {
            JournalEntryKind::Init { model, task_id, .. } => {
                assert_eq!(model, "test-model");
                assert_eq!(task_id.as_deref(), Some("test-task"));
            }
            _ => panic!("Expected Init entry"),
        }

        // Verify Message entry
        match &entries[1].kind {
            JournalEntryKind::Message { role, content, .. } => {
                assert_eq!(*role, Role::User);
                assert_eq!(content.len(), 1);
            }
            _ => panic!("Expected Message entry"),
        }
    }

    #[test]
    fn test_journal_resume_sequence() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("conversation.jsonl");

        // Write 3 entries
        {
            let mut journal = Journal::open(&path).unwrap();
            journal
                .append(JournalEntryKind::Init {
                    model: "m".to_string(),
                    provider: "p".to_string(),
                    system_prompt: "s".to_string(),
                    tools: vec![],
                    task_id: None,
                })
                .unwrap();
            journal
                .append(JournalEntryKind::Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Hi".to_string(),
                    }],
                    usage: None,
                    response_id: None,
                    stop_reason: None,
                })
                .unwrap();
            journal
                .append(JournalEntryKind::Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "Hello!".to_string(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        ..Usage::default()
                    }),
                    response_id: Some("resp-1".to_string()),
                    stop_reason: Some(StopReason::EndTurn),
                })
                .unwrap();
            assert_eq!(journal.seq(), 3);
        }

        // Reopen — should continue from seq 3
        let mut journal = Journal::open(&path).unwrap();
        assert_eq!(journal.seq(), 3);

        journal
            .append(JournalEntryKind::Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Follow-up".to_string(),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
        assert_eq!(journal.seq(), 4);

        let entries = Journal::read_all(&path).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[3].seq, 4);
    }

    #[test]
    fn test_journal_survives_crash() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("conversation.jsonl");

        // Write some entries
        {
            let mut journal = Journal::open(&path).unwrap();
            journal
                .append(JournalEntryKind::Init {
                    model: "m".to_string(),
                    provider: "p".to_string(),
                    system_prompt: "s".to_string(),
                    tools: vec![],
                    task_id: Some("crash-test".to_string()),
                })
                .unwrap();
            journal
                .append(JournalEntryKind::Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "First message".to_string(),
                    }],
                    usage: None,
                    response_id: None,
                    stop_reason: None,
                })
                .unwrap();
            journal
                .append(JournalEntryKind::Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "Response".to_string(),
                    }],
                    usage: Some(Usage {
                        input_tokens: 20,
                        output_tokens: 10,
                        ..Usage::default()
                    }),
                    response_id: Some("resp-1".to_string()),
                    stop_reason: Some(StopReason::ToolUse),
                })
                .unwrap();
            // Simulate crash: drop without writing End entry
            // Also append a partial/corrupted line to simulate mid-write crash
        }

        // Append a corrupted line (simulating a crash mid-write)
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(file, "{{\"seq\":4,\"timestamp\":\"broken").unwrap();
        }

        // Read back — should get 3 good entries, skip the corrupted one
        let entries = Journal::read_all(&path).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].seq, 1);
        assert_eq!(entries[2].seq, 3);

        // No End entry — conversation was interrupted
        assert!(
            !entries
                .iter()
                .any(|e| matches!(e.kind, JournalEntryKind::End { .. }))
        );

        // Can resume from this state
        let mut journal = Journal::open(&path).unwrap();
        // seq should be 3 (last valid entry), not 4 (the corrupted one)
        assert_eq!(journal.seq(), 3);

        // Can continue appending
        journal
            .append(JournalEntryKind::Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Resumed".to_string(),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();
        assert_eq!(journal.seq(), 4);
    }

    #[test]
    fn test_journal_tool_execution_entries() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("conversation.jsonl");

        let mut journal = Journal::open(&path).unwrap();
        journal
            .append(JournalEntryKind::Init {
                model: "m".to_string(),
                provider: "p".to_string(),
                system_prompt: "s".to_string(),
                tools: vec![ToolDefinition {
                    name: "read_file".to_string(),
                    description: "Read a file".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                }],
                task_id: None,
            })
            .unwrap();

        // User message
        journal
            .append(JournalEntryKind::Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Read foo.rs".to_string(),
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();

        // Assistant responds with tool_use
        journal
            .append(JournalEntryKind::Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "tu-1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "foo.rs"}),
                }],
                usage: Some(Usage {
                    input_tokens: 50,
                    output_tokens: 30,
                    ..Usage::default()
                }),
                response_id: Some("resp-1".to_string()),
                stop_reason: Some(StopReason::ToolUse),
            })
            .unwrap();

        // Tool execution
        journal
            .append(JournalEntryKind::ToolExecution {
                tool_use_id: "tu-1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "foo.rs"}),
                output: "fn main() {}".to_string(),
                is_error: false,
                duration_ms: 15,
            })
            .unwrap();

        // User message with tool result
        journal
            .append(JournalEntryKind::Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "tu-1".to_string(),
                    content: "fn main() {}".to_string(),
                    is_error: false,
                }],
                usage: None,
                response_id: None,
                stop_reason: None,
            })
            .unwrap();

        let entries = Journal::read_all(&path).unwrap();
        assert_eq!(entries.len(), 5);

        // Verify the ToolExecution entry
        match &entries[3].kind {
            JournalEntryKind::ToolExecution {
                tool_use_id,
                name,
                duration_ms,
                ..
            } => {
                assert_eq!(tool_use_id, "tu-1");
                assert_eq!(name, "read_file");
                assert_eq!(*duration_ms, 15);
            }
            _ => panic!("Expected ToolExecution entry at index 3"),
        }
    }

    #[test]
    fn test_journal_end_entry() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("conversation.jsonl");

        let mut journal = Journal::open(&path).unwrap();
        journal
            .append(JournalEntryKind::Init {
                model: "m".to_string(),
                provider: "p".to_string(),
                system_prompt: "s".to_string(),
                tools: vec![],
                task_id: None,
            })
            .unwrap();

        journal
            .append(JournalEntryKind::End {
                reason: EndReason::Complete,
                total_usage: Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Usage::default()
                },
                turns: 3,
            })
            .unwrap();

        let entries = Journal::read_all(&path).unwrap();
        assert_eq!(entries.len(), 2);
        match &entries[1].kind {
            JournalEntryKind::End {
                reason,
                total_usage,
                turns,
            } => {
                assert!(matches!(reason, EndReason::Complete));
                assert_eq!(total_usage.input_tokens, 100);
                assert_eq!(*turns, 3);
            }
            _ => panic!("Expected End entry"),
        }
    }

    #[test]
    fn test_journal_path_construction() {
        let dir = Path::new("/tmp/.workgraph");
        let path = journal_path(dir, "my-task-id");
        assert_eq!(
            path,
            Path::new("/tmp/.workgraph/output/my-task-id/conversation.jsonl")
        );
    }

    #[test]
    fn test_journal_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("conversation.jsonl");

        // Create an empty file
        File::create(&path).unwrap();

        let entries = Journal::read_all(&path).unwrap();
        assert!(entries.is_empty());

        let mut journal = Journal::open(&path).unwrap();
        assert_eq!(journal.seq(), 0);

        journal
            .append(JournalEntryKind::Init {
                model: "m".to_string(),
                provider: "p".to_string(),
                system_prompt: "s".to_string(),
                tools: vec![],
                task_id: None,
            })
            .unwrap();
        assert_eq!(journal.seq(), 1);
    }

    #[test]
    fn test_journal_entry_serialization_format() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("conversation.jsonl");

        let mut journal = Journal::open(&path).unwrap();
        journal
            .append(JournalEntryKind::Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Hello".to_string(),
                }],
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_creation_input_tokens: None,
                    cache_read_input_tokens: None,
                }),
                response_id: Some("msg-123".to_string()),
                stop_reason: Some(StopReason::EndTurn),
            })
            .unwrap();

        // Read raw JSON to verify format
        let raw = std::fs::read_to_string(&path).unwrap();
        let val: serde_json::Value = serde_json::from_str(raw.trim()).unwrap();

        // entry_type should be flattened at the top level
        assert_eq!(val["entry_type"], "message");
        assert_eq!(val["seq"], 1);
        assert!(val["timestamp"].is_string());
        assert_eq!(val["role"], "assistant");
        assert_eq!(val["response_id"], "msg-123");
        assert_eq!(val["stop_reason"], "end_turn");
        assert_eq!(val["usage"]["input_tokens"], 10);
    }

    #[test]
    fn test_journal_creates_parent_directories() {
        let tmp = TempDir::new().unwrap();
        let path = tmp
            .path()
            .join("output")
            .join("task-id")
            .join("conversation.jsonl");

        // Parent dirs don't exist yet
        assert!(!path.parent().unwrap().exists());

        let mut journal = Journal::open(&path).unwrap();
        assert!(path.parent().unwrap().exists());

        journal
            .append(JournalEntryKind::Init {
                model: "m".to_string(),
                provider: "p".to_string(),
                system_prompt: "s".to_string(),
                tools: vec![],
                task_id: Some("task-id".to_string()),
            })
            .unwrap();

        let entries = Journal::read_all(&path).unwrap();
        assert_eq!(entries.len(), 1);
    }
}
