//! Resume protocol for the native executor.
//!
//! When an agent dies mid-task and the task is retried, the new agent can pick
//! up from the conversation journal rather than starting from scratch.
//!
//! Protocol:
//! 1. Check for existing journal at `.workgraph/output/<task-id>/conversation.jsonl`
//! 2. Load entries, reconstruct message history
//! 3. If journal exceeds budget, compact older turns (summarize)
//! 4. Detect stale tool results (files changed since journal was written)
//! 5. Inject resume context as a system annotation

use std::path::Path;

use anyhow::{Context, Result};

use super::client::{ContentBlock, Message, Role};
use super::journal::{JournalEntry, JournalEntryKind};

/// Default budget: if the journal's estimated tokens exceed this percentage of
/// the context window, compact older turns.
const DEFAULT_BUDGET_PCT: f64 = 0.50;

/// Rough chars-per-token estimate for budget calculation.
const CHARS_PER_TOKEN: usize = 4;

/// Number of recent message pairs to keep verbatim during compaction.
const KEEP_RECENT_MESSAGES: usize = 6;

/// Result of loading and processing a journal for resume.
#[derive(Debug)]
pub struct ResumeData {
    /// Reconstructed conversation messages (ready to send to the API).
    pub messages: Vec<Message>,
    /// The system prompt from the original Init entry.
    pub system_prompt: Option<String>,
    /// Number of entries in the original journal.
    pub original_entry_count: usize,
    /// Whether the journal was compacted during resume.
    pub was_compacted: bool,
    /// Stale state annotations (files that changed since journal was written).
    pub stale_annotations: Vec<String>,
    /// The sequence number of the last entry in the journal.
    pub last_seq: u64,
}

/// Configuration for the resume protocol.
#[derive(Debug, Clone)]
pub struct ResumeConfig {
    /// Budget percentage: compact if estimated tokens exceed this fraction of context window.
    pub budget_pct: f64,
    /// Estimated context window size in tokens.
    pub context_window_tokens: usize,
}

impl Default for ResumeConfig {
    fn default() -> Self {
        Self {
            budget_pct: DEFAULT_BUDGET_PCT,
            // 200k tokens is a common large-context default
            context_window_tokens: 200_000,
        }
    }
}

/// Load a conversation journal and prepare resume data.
///
/// Returns `None` if the journal doesn't exist or is empty.
pub fn load_resume_data(
    journal_path: &Path,
    working_dir: &Path,
    config: &ResumeConfig,
) -> Result<Option<ResumeData>> {
    if !journal_path.exists() {
        return Ok(None);
    }

    let entries = super::journal::Journal::read_all(journal_path).with_context(|| {
        format!(
            "Failed to read journal for resume: {}",
            journal_path.display()
        )
    })?;

    if entries.is_empty() {
        return Ok(None);
    }

    let original_entry_count = entries.len();
    let last_seq = entries.last().map(|e| e.seq).unwrap_or(0);

    // Extract system prompt from Init entry
    let system_prompt = entries.iter().find_map(|e| match &e.kind {
        JournalEntryKind::Init { system_prompt, .. } => Some(system_prompt.clone()),
        _ => None,
    });

    // Reconstruct messages from journal entries
    let messages = reconstruct_messages(&entries);

    if messages.is_empty() {
        return Ok(None);
    }

    // Detect stale state
    let stale_annotations = detect_stale_state(&entries, working_dir);

    // Check budget and compact if needed
    let estimated_tokens = estimate_tokens(&messages);
    let budget_tokens = (config.context_window_tokens as f64 * config.budget_pct) as usize;
    let (messages, was_compacted) = if estimated_tokens > budget_tokens {
        (compact_messages(messages, budget_tokens), true)
    } else {
        (messages, false)
    };

    Ok(Some(ResumeData {
        messages,
        system_prompt,
        original_entry_count,
        was_compacted,
        stale_annotations,
        last_seq,
    }))
}

/// Reconstruct `Vec<Message>` from journal entries.
///
/// Extracts Message entries (user/assistant), skipping Init, ToolExecution,
/// Compaction, and End entries. ToolExecution is metadata — the actual tool
/// results appear in the subsequent User message as ToolResult content blocks.
fn reconstruct_messages(entries: &[JournalEntry]) -> Vec<Message> {
    let mut messages = Vec::new();

    for entry in entries {
        match &entry.kind {
            JournalEntryKind::Message { role, content, .. } => {
                messages.push(Message {
                    role: *role,
                    content: content.clone(),
                });
            }
            JournalEntryKind::Compaction { summary, .. } => {
                // A prior compaction: inject the summary as a user message
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: format!(
                            "[Resume: Prior conversation was compacted. Summary of earlier work:]\n{}",
                            summary
                        ),
                    }],
                });
            }
            // Init, ToolExecution, End — skip
            _ => {}
        }
    }

    messages
}

/// Estimate total tokens in a message list using a rough heuristic.
fn estimate_tokens(messages: &[Message]) -> usize {
    let total_chars: usize = messages
        .iter()
        .map(|m| {
            m.content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.len(),
                    ContentBlock::ToolUse { input, name, .. } => {
                        name.len() + input.to_string().len()
                    }
                    ContentBlock::ToolResult { content, .. } => content.len(),
                })
                .sum::<usize>()
        })
        .sum();
    total_chars / CHARS_PER_TOKEN
}

/// Compact messages to fit within the token budget.
///
/// Strategy: keep the first message (context) and the last N messages verbatim.
/// Replace everything in between with a summary of the compacted region.
fn compact_messages(messages: Vec<Message>, _budget_tokens: usize) -> Vec<Message> {
    if messages.len() <= KEEP_RECENT_MESSAGES + 1 {
        // Too few messages to compact
        return messages;
    }

    let split_point = messages.len().saturating_sub(KEEP_RECENT_MESSAGES);

    // Summarize the older messages
    let older = &messages[..split_point];
    let summary = summarize_messages(older);

    let mut compacted = Vec::with_capacity(KEEP_RECENT_MESSAGES + 1);

    // Inject the compaction summary as a user message
    compacted.push(Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: format!(
                "[Resume: This conversation is being resumed from a journal. \
                 The first {} messages were compacted into this summary:]\n\n{}",
                split_point, summary
            ),
        }],
    });

    // Keep the recent messages verbatim
    compacted.extend_from_slice(&messages[split_point..]);

    // Ensure the conversation starts with a user message (required by API).
    // The compaction summary is already a user message, so this should be fine,
    // but verify the alternation is valid.
    ensure_valid_alternation(&mut compacted);

    compacted
}

/// Generate a text summary of a block of messages.
///
/// This is a local, synchronous summarizer — it extracts key information
/// rather than calling an LLM. For deeper summarization, the compaction
/// entry type in the journal can be used by an external process.
fn summarize_messages(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    let mut tool_calls_seen = Vec::new();
    let mut key_texts = Vec::new();

    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    // Keep short texts, truncate long ones
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        if trimmed.len() > 200 {
                            key_texts.push(format!(
                                "{}...",
                                &trimmed[..trimmed.floor_char_boundary(200)]
                            ));
                        } else {
                            key_texts.push(trimmed.to_string());
                        }
                    }
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    // Summarize tool calls
                    let input_str = input.to_string();
                    let input_summary = if input_str.len() > 100 {
                        format!("{}...", &input_str[..input_str.floor_char_boundary(100)])
                    } else {
                        input_str
                    };
                    tool_calls_seen.push(format!("{}({})", name, input_summary));
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    if *is_error {
                        let preview = if content.len() > 100 {
                            format!("{}...", &content[..content.floor_char_boundary(100)])
                        } else {
                            content.clone()
                        };
                        parts.push(format!("Tool error: {}", preview));
                    }
                }
            }
        }
    }

    let mut summary = String::new();

    if !tool_calls_seen.is_empty() {
        summary.push_str(&format!("Tools called: {}\n", tool_calls_seen.join(", ")));
    }

    if !parts.is_empty() {
        summary.push_str(&format!("Notable events: {}\n", parts.join("; ")));
    }

    if !key_texts.is_empty() {
        // Include only the first few and last few key texts
        let max_texts = 4;
        if key_texts.len() <= max_texts * 2 {
            summary.push_str(&format!("Key messages:\n{}", key_texts.join("\n")));
        } else {
            let first: Vec<_> = key_texts[..max_texts].to_vec();
            let last: Vec<_> = key_texts[key_texts.len() - max_texts..].to_vec();
            summary.push_str(&format!(
                "Key messages (first {max_texts}):\n{}\n\n[...{} messages omitted...]\n\nKey messages (last {max_texts}):\n{}",
                first.join("\n"),
                key_texts.len() - max_texts * 2,
                last.join("\n")
            ));
        }
    }

    if summary.is_empty() {
        summary = format!(
            "Prior conversation had {} messages (no significant text content).",
            messages.len()
        );
    }

    summary
}

/// Ensure the message list has valid user/assistant alternation.
///
/// The API requires messages to start with a user message and alternate.
/// If we have two consecutive same-role messages, merge them.
fn ensure_valid_alternation(messages: &mut Vec<Message>) {
    if messages.is_empty() {
        return;
    }

    // Ensure first message is from user
    if messages[0].role != Role::User {
        messages.insert(
            0,
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "[Resume: Conversation continued from prior session.]".to_string(),
                }],
            },
        );
    }

    // Merge consecutive same-role messages
    let mut i = 1;
    while i < messages.len() {
        if messages[i].role == messages[i - 1].role {
            let content = messages[i].content.clone();
            messages[i - 1].content.extend(content);
            messages.remove(i);
        } else {
            i += 1;
        }
    }
}

/// Detect tool results in the journal that may be stale.
///
/// Checks `read_file` tool executions: if the file still exists, compare a
/// hash of the content at journal-time vs now. Also checks `write_file`
/// executions to see if the file was modified after the journal recorded it.
fn detect_stale_state(entries: &[JournalEntry], working_dir: &Path) -> Vec<String> {
    let mut annotations = Vec::new();
    let mut checked_paths = std::collections::HashSet::new();

    for entry in entries {
        if let JournalEntryKind::ToolExecution {
            name,
            input,
            output,
            is_error,
            ..
        } = &entry.kind
        {
            if *is_error {
                continue;
            }

            match name.as_str() {
                "read_file" => {
                    if let Some(path_str) = input.get("path").and_then(|v| v.as_str()) {
                        // Resolve relative paths against working dir
                        let file_path = if Path::new(path_str).is_absolute() {
                            std::path::PathBuf::from(path_str)
                        } else {
                            working_dir.join(path_str)
                        };

                        if checked_paths.contains(&file_path) {
                            continue;
                        }
                        checked_paths.insert(file_path.clone());

                        if !file_path.exists() {
                            annotations.push(format!(
                                "STALE: File '{}' was read in prior session but no longer exists",
                                path_str
                            ));
                        } else if let Ok(current_content) = std::fs::read_to_string(&file_path) {
                            // Compare content (output is the file content the agent saw)
                            if content_differs(output, &current_content) {
                                annotations.push(format!(
                                    "STALE: File '{}' has changed since it was last read in the prior session",
                                    path_str
                                ));
                            }
                        }
                    }
                }
                "write_file" => {
                    if let Some(path_str) = input.get("path").and_then(|v| v.as_str()) {
                        let file_path = if Path::new(path_str).is_absolute() {
                            std::path::PathBuf::from(path_str)
                        } else {
                            working_dir.join(path_str)
                        };

                        if checked_paths.contains(&file_path) {
                            continue;
                        }
                        checked_paths.insert(file_path.clone());

                        if !file_path.exists() {
                            annotations.push(format!(
                                "STALE: File '{}' was written in prior session but no longer exists",
                                path_str
                            ));
                        } else if let Some(written_content) =
                            input.get("content").and_then(|v| v.as_str())
                            && let Ok(current_content) = std::fs::read_to_string(&file_path)
                            && current_content != written_content
                        {
                            annotations.push(format!(
                                "STALE: File '{}' was written in prior session but has been modified since",
                                path_str
                            ));
                        }
                    }
                }
                "bash" => {
                    // Bash commands can have side effects but we can't easily check
                    // them — just note if the journal contains bash calls
                    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                        // Only annotate file-mutating commands
                        if cmd.contains("git ")
                            || cmd.contains("cargo ")
                            || cmd.contains("rm ")
                            || cmd.contains("mv ")
                            || cmd.contains("cp ")
                        {
                            annotations.push(format!(
                                "NOTE: Prior session ran bash command: {} (effects may have changed)",
                                if cmd.len() > 100 {
                                    format!("{}...", &cmd[..cmd.floor_char_boundary(100)])
                                } else {
                                    cmd.to_string()
                                }
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    annotations
}

/// Check if the journal-recorded content differs from current content.
///
/// Tool output may have line numbers or other formatting, so we use a
/// relaxed comparison: if the output is a prefix/suffix of the current
/// content or vice versa, consider it unchanged.
fn content_differs(journal_output: &str, current_content: &str) -> bool {
    if journal_output == current_content {
        return false;
    }

    // The read_file tool may return content with line numbers or truncation markers.
    // Normalize by stripping line number prefixes if present.
    let journal_lines: Vec<&str> = journal_output.lines().collect();
    let current_lines: Vec<&str> = current_content.lines().collect();

    // Quick check: if line counts are very different, content changed
    if (journal_lines.len() as isize - current_lines.len() as isize).unsigned_abs() > 5 {
        return true;
    }

    // Strip potential line number prefixes (e.g., "   1\t" format from cat -n)
    fn strip_line_number(line: &str) -> &str {
        // Pattern: optional spaces, digits, tab, then content
        if let Some(pos) = line.find('\t') {
            let prefix = &line[..pos];
            if prefix.trim().chars().all(|c| c.is_ascii_digit()) {
                return &line[pos + 1..];
            }
        }
        line
    }

    let journal_stripped: Vec<&str> = journal_lines.iter().map(|l| strip_line_number(l)).collect();
    let current_stripped: Vec<&str> = current_lines.iter().map(|l| strip_line_number(l)).collect();

    journal_stripped != current_stripped
}

/// Build the resume context injection message.
///
/// This message is prepended to the conversation to inform the agent
/// about the resume and any stale state.
pub fn build_resume_annotation(resume_data: &ResumeData) -> String {
    let mut parts = vec![format!(
        "IMPORTANT: This task is being RESUMED from a prior agent session that was interrupted. \
         You have {} messages of prior conversation context loaded. \
         Continue from where the previous agent left off — do NOT restart work that was already completed.",
        resume_data.messages.len()
    )];

    if resume_data.was_compacted {
        parts.push(
            "The conversation history was compacted (older turns summarized) to fit within context limits."
                .to_string(),
        );
    }

    if !resume_data.stale_annotations.is_empty() {
        parts.push(format!(
            "\nWARNING — The following state changes were detected since the prior session:\n{}",
            resume_data
                .stale_annotations
                .iter()
                .map(|a| format!("  - {}", a))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        parts.push("Re-read any affected files before relying on prior tool results.".to_string());
    }

    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::native::client::Usage;
    use crate::executor::native::journal::{Journal, JournalEntryKind};
    use tempfile::TempDir;

    fn make_journal_with_messages(dir: &Path, messages: &[(Role, &str)]) -> std::path::PathBuf {
        let path = dir.join("conversation.jsonl");
        let mut journal = Journal::open(&path).unwrap();

        journal
            .append(JournalEntryKind::Init {
                model: "test-model".to_string(),
                provider: "test-provider".to_string(),
                system_prompt: "You are a test agent.".to_string(),
                tools: vec![],
                task_id: Some("test-task".to_string()),
            })
            .unwrap();

        for (role, text) in messages {
            journal
                .append(JournalEntryKind::Message {
                    role: *role,
                    content: vec![ContentBlock::Text {
                        text: text.to_string(),
                    }],
                    usage: if *role == Role::Assistant {
                        Some(Usage {
                            input_tokens: 10,
                            output_tokens: 5,
                            ..Usage::default()
                        })
                    } else {
                        None
                    },
                    response_id: if *role == Role::Assistant {
                        Some("resp-1".to_string())
                    } else {
                        None
                    },
                    stop_reason: None,
                })
                .unwrap();
        }

        path
    }

    #[test]
    fn test_reconstruct_messages_basic() {
        let tmp = TempDir::new().unwrap();
        let path = make_journal_with_messages(
            tmp.path(),
            &[
                (Role::User, "Hello"),
                (Role::Assistant, "Hi there!"),
                (Role::User, "Do something"),
                (Role::Assistant, "Done!"),
            ],
        );

        let entries = Journal::read_all(&path).unwrap();
        let messages = reconstruct_messages(&entries);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[1].role, Role::Assistant);
        assert_eq!(messages[2].role, Role::User);
        assert_eq!(messages[3].role, Role::Assistant);
    }

    #[test]
    fn test_load_resume_data_nonexistent() {
        let result = load_resume_data(
            Path::new("/nonexistent/conversation.jsonl"),
            Path::new("/tmp"),
            &ResumeConfig::default(),
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_load_resume_data_basic() {
        let tmp = TempDir::new().unwrap();
        let path = make_journal_with_messages(
            tmp.path(),
            &[
                (Role::User, "Start task"),
                (Role::Assistant, "Working on it"),
            ],
        );

        let resume = load_resume_data(&path, tmp.path(), &ResumeConfig::default())
            .unwrap()
            .expect("Should load resume data");

        assert_eq!(resume.messages.len(), 2);
        assert_eq!(resume.original_entry_count, 3); // Init + 2 messages
        assert!(!resume.was_compacted);
        assert!(resume.stale_annotations.is_empty());
        assert_eq!(
            resume.system_prompt.as_deref(),
            Some("You are a test agent.")
        );
    }

    #[test]
    fn test_compact_messages_small() {
        // Too few messages to compact
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Hello".to_string(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "Hi".to_string(),
                }],
            },
        ];

        let compacted = compact_messages(messages.clone(), 100);
        assert_eq!(compacted.len(), 2); // No change
    }

    #[test]
    fn test_compact_messages_large() {
        // Create many messages
        let mut messages = Vec::new();
        for i in 0..20 {
            messages.push(Message {
                role: if i % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                content: vec![ContentBlock::Text {
                    text: format!("Message {}", i),
                }],
            });
        }

        let compacted = compact_messages(messages, 100);

        // Should have: 1 summary + KEEP_RECENT_MESSAGES recent
        assert!(compacted.len() <= KEEP_RECENT_MESSAGES + 1);

        // First message should be the compaction summary
        match &compacted[0].content[0] {
            ContentBlock::Text { text } => {
                assert!(
                    text.contains("compacted"),
                    "Summary should mention compaction: {}",
                    text
                );
            }
            _ => panic!("Expected text content in compaction summary"),
        }
    }

    #[test]
    fn test_detect_stale_read_file() {
        let tmp = TempDir::new().unwrap();

        // Create a file
        let test_file = tmp.path().join("foo.rs");
        std::fs::write(&test_file, "fn main() {}").unwrap();

        // Create a journal entry for reading the file
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
            .append(JournalEntryKind::ToolExecution {
                tool_use_id: "tu-1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": test_file.to_str().unwrap()}),
                output: "fn main() {}".to_string(),
                is_error: false,
                duration_ms: 10,
            })
            .unwrap();

        let entries = Journal::read_all(&path).unwrap();

        // File unchanged — no stale annotations
        let annotations = detect_stale_state(&entries, tmp.path());
        assert!(
            annotations.is_empty(),
            "No stale annotations expected: {:?}",
            annotations
        );

        // Now modify the file
        std::fs::write(&test_file, "fn main() { println!(\"hello\"); }").unwrap();

        let annotations = detect_stale_state(&entries, tmp.path());
        assert_eq!(annotations.len(), 1);
        assert!(annotations[0].contains("STALE"));
        assert!(annotations[0].contains("foo.rs"));
    }

    #[test]
    fn test_detect_stale_deleted_file() {
        let tmp = TempDir::new().unwrap();

        let test_file = tmp.path().join("deleted.rs");
        std::fs::write(&test_file, "content").unwrap();

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
            .append(JournalEntryKind::ToolExecution {
                tool_use_id: "tu-1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": test_file.to_str().unwrap()}),
                output: "content".to_string(),
                is_error: false,
                duration_ms: 10,
            })
            .unwrap();

        // Delete the file
        std::fs::remove_file(&test_file).unwrap();

        let entries = Journal::read_all(&path).unwrap();
        let annotations = detect_stale_state(&entries, tmp.path());
        assert_eq!(annotations.len(), 1);
        assert!(annotations[0].contains("no longer exists"));
    }

    #[test]
    fn test_ensure_valid_alternation() {
        // Two consecutive user messages should be merged
        let mut messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "A".to_string(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "B".to_string(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "C".to_string(),
                }],
            },
        ];

        ensure_valid_alternation(&mut messages);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert_eq!(messages[0].content.len(), 2); // Merged
        assert_eq!(messages[1].role, Role::Assistant);
    }

    #[test]
    fn test_estimate_tokens() {
        let text = "Hello world, this is a test message.";
        let messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }];

        let tokens = estimate_tokens(&messages);
        assert_eq!(tokens, text.len() / CHARS_PER_TOKEN);
    }

    #[test]
    fn test_build_resume_annotation() {
        let resume_data = ResumeData {
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Hello".to_string(),
                    }],
                },
                Message {
                    role: Role::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "Hi".to_string(),
                    }],
                },
            ],
            system_prompt: Some("Test prompt".to_string()),
            original_entry_count: 3,
            was_compacted: false,
            stale_annotations: vec!["STALE: File 'foo.rs' has changed".to_string()],
            last_seq: 3,
        };

        let annotation = build_resume_annotation(&resume_data);
        assert!(annotation.contains("RESUMED"));
        assert!(annotation.contains("2 messages"));
        assert!(annotation.contains("STALE"));
        assert!(annotation.contains("foo.rs"));
    }

    #[test]
    fn test_build_resume_annotation_compacted() {
        let resume_data = ResumeData {
            messages: vec![],
            system_prompt: None,
            original_entry_count: 50,
            was_compacted: true,
            stale_annotations: vec![],
            last_seq: 50,
        };

        let annotation = build_resume_annotation(&resume_data);
        assert!(annotation.contains("compacted"));
    }

    #[test]
    fn test_content_differs_identical() {
        assert!(!content_differs("fn main() {}", "fn main() {}"));
    }

    #[test]
    fn test_content_differs_changed() {
        assert!(content_differs("fn main() {}", "fn main() { todo!() }"));
    }

    #[test]
    fn test_content_differs_with_line_numbers() {
        // cat -n style output: single line with line number prefix
        let journal_output = "     1\tfn main() {}";
        let current = "fn main() {}";
        assert!(!content_differs(journal_output, current));

        // Multi-line with line numbers
        let journal_output_multi = "     1\tfn main() {\n     2\t    println!(\"hi\");\n     3\t}";
        let current_multi = "fn main() {\n    println!(\"hi\");\n}";
        assert!(!content_differs(journal_output_multi, current_multi));
    }
}
