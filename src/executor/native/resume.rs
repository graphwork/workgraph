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

use super::client::{ContentBlock, Message, Role, ToolDefinition};
use super::journal::{JournalEntry, JournalEntryKind};

/// Estimate the fixed overhead tokens the agent carries independent of
/// message history: system prompt + serialized tool definitions + the
/// completion reservation (`max_tokens`).
///
/// Used by [`ContextBudget::with_overhead`] to get accurate pressure
/// thresholds, especially on small-context models where the overhead is
/// a large fraction of the window.
pub fn estimate_agent_overhead(
    system_prompt: &str,
    tool_defs: &[ToolDefinition],
    max_tokens: u32,
    chars_per_token: f64,
) -> usize {
    let system_chars = system_prompt.len();
    let tool_chars: usize = tool_defs
        .iter()
        .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
        .sum();
    let static_tokens = ((system_chars + tool_chars) as f64 / chars_per_token) as usize;
    static_tokens + max_tokens as usize
}

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
                    ContentBlock::Thinking { thinking, .. } => thinking.len(),
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
                ContentBlock::Thinking { .. } => {
                    // Thinking blocks are not summarized — they are internal reasoning
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

/// Maximum word count for session summaries.
const MAX_SUMMARY_WORDS: usize = 500;

/// Extract a structured session summary from messages.
///
/// Produces a Markdown summary with sections: Key Findings, Decisions,
/// Files Modified, Current State. Capped at `MAX_SUMMARY_WORDS` words.
pub fn extract_session_summary(messages: &[Message]) -> String {
    let mut findings = Vec::new();
    let mut decisions = Vec::new();
    let mut files_modified = std::collections::HashSet::new();
    let mut last_assistant_text = String::new();
    let mut tool_calls = Vec::new();

    for msg in messages {
        for block in &msg.content {
            match block {
                ContentBlock::Text { text } => {
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if msg.role == Role::Assistant {
                        last_assistant_text = trimmed.to_string();
                        // Extract decisions (lines that look like decisions/conclusions)
                        for line in trimmed.lines() {
                            let l = line.trim();
                            if (l.starts_with("- [x]")
                                || l.starts_with("Decision:")
                                || l.starts_with("Decided")
                                || l.contains("will ")
                                || l.contains("chose ")
                                || l.contains("decided "))
                                && l.len() > 10
                            {
                                decisions.push(truncate_str(l, 150));
                            }
                        }
                    } else {
                        // User messages may contain findings/context
                        for line in trimmed.lines() {
                            let l = line.trim();
                            if (l.starts_with("ERROR")
                                || l.starts_with("Warning")
                                || l.starts_with("Found")
                                || l.starts_with("Note:"))
                                && l.len() > 10
                            {
                                findings.push(truncate_str(l, 150));
                            }
                        }
                    }
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    tool_calls.push(name.clone());
                    // Track file modifications
                    match name.as_str() {
                        "write_file" | "edit_file" | "create_file" => {
                            if let Some(path) = input.get("path").and_then(|v| v.as_str()) {
                                files_modified.insert(path.to_string());
                            }
                        }
                        "bash" => {
                            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                                // Detect file-writing bash commands
                                if cmd.contains("git add") || cmd.contains("git commit") {
                                    // Extract file paths from git commands
                                    for part in cmd.split_whitespace() {
                                        if part.contains('.')
                                            && !part.starts_with('-')
                                            && part.len() > 2
                                        {
                                            files_modified.insert(part.to_string());
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    if *is_error && content.len() > 10 {
                        findings.push(format!("Error: {}", truncate_str(content, 120)));
                    }
                }
                ContentBlock::Thinking { .. } => {
                    // Thinking blocks are not included in session summaries
                }
            }
        }
    }

    // Build the summary
    let mut parts = Vec::new();

    parts.push("# Session Summary\n".to_string());

    if !findings.is_empty() {
        parts.push("## Key Findings".to_string());
        for f in findings.iter().take(10) {
            parts.push(format!("- {}", f));
        }
        parts.push(String::new());
    }

    if !decisions.is_empty() {
        parts.push("## Decisions".to_string());
        for d in decisions.iter().take(10) {
            parts.push(format!("- {}", d));
        }
        parts.push(String::new());
    }

    if !files_modified.is_empty() {
        parts.push("## Files Modified".to_string());
        let mut files: Vec<_> = files_modified.into_iter().collect();
        files.sort();
        for f in &files {
            parts.push(format!("- `{}`", f));
        }
        parts.push(String::new());
    }

    if !tool_calls.is_empty() {
        // Deduplicated count of tool calls
        let mut counts = std::collections::HashMap::new();
        for t in &tool_calls {
            *counts.entry(t.as_str()).or_insert(0u32) += 1;
        }
        parts.push("## Tool Usage".to_string());
        let mut sorted: Vec<_> = counts.into_iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(&a.1));
        for (name, count) in sorted.iter().take(10) {
            parts.push(format!("- {}: {}x", name, count));
        }
        parts.push(String::new());
    }

    // Current state: last assistant message, truncated
    if !last_assistant_text.is_empty() {
        parts.push("## Current State".to_string());
        parts.push(truncate_str(&last_assistant_text, 300));
        parts.push(String::new());
    }

    let summary = parts.join("\n");

    // Enforce word limit
    let words: Vec<&str> = summary.split_whitespace().collect();
    if words.len() > MAX_SUMMARY_WORDS {
        words[..MAX_SUMMARY_WORDS].join(" ") + "\n[...truncated]"
    } else {
        summary
    }
}

/// Store a session summary to a file, creating parent directories as needed.
pub fn store_session_summary(path: &Path, summary: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create summary directory: {}", parent.display()))?;
    }
    std::fs::write(path, summary)
        .with_context(|| format!("Failed to write session summary: {}", path.display()))?;
    Ok(())
}

/// Load a session summary from a file, if it exists.
///
/// Returns `None` if the file does not exist.
pub fn load_session_summary(path: &Path) -> Result<Option<String>> {
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read session summary: {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(content))
}

/// Truncate a string to at most `max_len` characters, appending "..." if truncated.
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..s.floor_char_boundary(max_len)])
    }
}

/// The action the agent loop should take based on context pressure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextPressureAction {
    /// Context usage is within safe limits — no action needed.
    Ok,
    /// At 80%+ capacity — inject a warning into the next turn.
    Warning,
    /// At 90%+ capacity — emergency compaction needed (drop old tool results).
    EmergencyCompaction,
    /// At 95%+ capacity — clean exit (log progress, create continuation, exit).
    CleanExit,
}

/// Budget thresholds for tiered context pressure management.
///
/// The agent loop checks this after each turn to decide whether to warn,
/// compact, or exit gracefully before hitting the API's hard limit.
///
/// # Overhead accounting
///
/// The API budget is `system + tools + messages + max_tokens_reservation ≤ window_size`.
/// [`ContextBudget::estimate_tokens`] only counts message content, so
/// [`ContextBudget::check_pressure`] would underreport actual usage if used
/// alone. The `overhead_tokens` field captures the fixed cost of system prompt
/// + tool definitions + completion budget, computed once at agent init via
/// [`estimate_agent_overhead`], so pressure checks reflect real API usage.
#[derive(Debug, Clone)]
pub struct ContextBudget {
    /// Total context window size in tokens (from provider config).
    pub window_size: usize,
    /// Rough chars-per-token estimate (default 4.0).
    pub chars_per_token: f64,
    /// Fraction at which to inject a warning (default 0.70).
    pub warning_threshold: f64,
    /// Fraction at which to trigger emergency compaction (default 0.75).
    pub compact_threshold: f64,
    /// Fraction at which to trigger a clean exit (default 0.95).
    pub hard_limit: f64,
    /// Fixed overhead tokens that count against the window but are NOT in
    /// the `messages` vec: system prompt + tool definitions + completion
    /// reservation (`max_tokens`). Typically set once at agent init.
    pub overhead_tokens: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            window_size: 200_000,
            chars_per_token: 4.0,
            warning_threshold: 0.70,
            compact_threshold: 0.75,
            hard_limit: 0.95,
            overhead_tokens: 0,
        }
    }
}

impl ContextBudget {
    /// Create a ContextBudget with a specific window size and dynamic thresholds.
    ///
    /// Smaller context windows get tighter (earlier) thresholds since there's less
    /// room to recover from pressure:
    /// - Small  (< 64k):  warning=0.55, compact=0.65, hard_limit=0.85
    /// - Medium (64k–128k): warning=0.60, compact=0.70, hard_limit=0.90
    /// - Large  (≥ 128k):  warning=0.70, compact=0.75, hard_limit=0.95
    pub fn with_window_size(window_size: usize) -> Self {
        let (warning, compact, hard) = if window_size < 64_000 {
            (0.55, 0.65, 0.85)
        } else if window_size < 128_000 {
            (0.60, 0.70, 0.90)
        } else {
            (0.70, 0.75, 0.95)
        };
        Self {
            window_size,
            chars_per_token: 4.0,
            warning_threshold: warning,
            compact_threshold: compact,
            hard_limit: hard,
            overhead_tokens: 0,
        }
    }

    /// Set the fixed overhead tokens (system prompt + tool defs + completion reservation).
    ///
    /// Compaction/pressure decisions use `message_tokens + overhead_tokens` against
    /// `window_size`, so this must be set at agent init for accurate thresholds on
    /// small-context models where the overhead is a large fraction of the window.
    pub fn with_overhead(mut self, overhead_tokens: usize) -> Self {
        self.overhead_tokens = overhead_tokens;
        self
    }

    /// Estimate the current token count from a list of messages.
    pub fn estimate_tokens(&self, messages: &[Message]) -> usize {
        let total_chars: usize = messages
            .iter()
            .map(|m| {
                m.content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => text.len(),
                        ContentBlock::Thinking { thinking, .. } => thinking.len(),
                        ContentBlock::ToolUse { input, name, .. } => {
                            name.len() + input.to_string().len()
                        }
                        ContentBlock::ToolResult { content, .. } => content.len(),
                    })
                    .sum::<usize>()
            })
            .sum();
        (total_chars as f64 / self.chars_per_token) as usize
    }

    /// Estimated tokens for messages PLUS the fixed overhead (system + tools + completion).
    /// This is the value compared against `window_size` for pressure decisions.
    pub fn effective_tokens(&self, messages: &[Message]) -> usize {
        self.estimate_tokens(messages) + self.overhead_tokens
    }

    /// Check context pressure and return the appropriate action.
    ///
    /// Uses [`Self::effective_tokens`] (messages + overhead) vs. `window_size`
    /// so the ratio reflects actual API budget usage, not just message content.
    pub fn check_pressure(&self, messages: &[Message]) -> ContextPressureAction {
        let tokens = self.effective_tokens(messages);
        let ratio = tokens as f64 / self.window_size as f64;

        if ratio >= self.hard_limit {
            ContextPressureAction::CleanExit
        } else if ratio >= self.compact_threshold {
            ContextPressureAction::EmergencyCompaction
        } else if ratio >= self.warning_threshold {
            ContextPressureAction::Warning
        } else {
            ContextPressureAction::Ok
        }
    }

    /// Build the warning message injected at the warning threshold.
    pub fn warning_message(&self, messages: &[Message]) -> String {
        let msg_tokens = self.estimate_tokens(messages);
        let effective = msg_tokens + self.overhead_tokens;
        let pct = (effective as f64 / self.window_size as f64) * 100.0;
        format!(
            "⚠️ CONTEXT PRESSURE WARNING: You're at {:.0}% context capacity \
             ({} messages + {} overhead = {} / {} estimated tokens). \
             Consider logging progress via `wg log` and completing the current subtask.",
            pct, msg_tokens, self.overhead_tokens, effective, self.window_size
        )
    }

    /// Collect (msg_idx, block_idx) for every `ToolResult` block whose content
    /// exceeds `size_threshold` bytes, walking messages in order.
    fn locate_large_tool_results(
        messages: &[Message],
        size_threshold: usize,
    ) -> Vec<(usize, usize)> {
        let mut locations = Vec::new();
        for (mi, msg) in messages.iter().enumerate() {
            for (bi, block) in msg.content.iter().enumerate() {
                if let ContentBlock::ToolResult { content, .. } = block
                    && content.len() > size_threshold
                {
                    locations.push((mi, bi));
                }
            }
        }
        locations
    }

    /// Select which tool-result locations to **keep verbatim**: the last
    /// `keep_recent_tool_results` entries from a list in chronological order.
    fn keep_set(
        locations: &[(usize, usize)],
        keep_recent_tool_results: usize,
    ) -> std::collections::HashSet<(usize, usize)> {
        let keep_count = keep_recent_tool_results.min(locations.len());
        let start = locations.len() - keep_count;
        locations[start..].iter().copied().collect()
    }

    /// Perform emergency compaction: walk ALL messages and shrink any
    /// `ToolResult` block larger than 200 bytes, EXCEPT the last
    /// `keep_recent_tool_results` of them, which are preserved verbatim as
    /// the model's working memory.
    ///
    /// This is **position-independent by design** — the big content in a
    /// chatty file-reading workload lives in recent positions (one tool call
    /// per file), so a position-based "keep last N messages" rule would
    /// protect the very content we need to shrink. By counting tool-result
    /// occurrences instead of messages, we always compact the earliest tool
    /// results first regardless of where they sit in the message vec.
    ///
    /// Message count is unchanged so tool_use/tool_result pairing stays
    /// intact. Token reduction comes from shrinking `ToolResult::content`.
    pub fn emergency_compact(
        messages: Vec<Message>,
        keep_recent_tool_results: usize,
    ) -> Vec<Message> {
        let locations = Self::locate_large_tool_results(&messages, 200);
        if locations.is_empty() {
            return messages;
        }
        let keep = Self::keep_set(&locations, keep_recent_tool_results);

        let mut compacted = Vec::with_capacity(messages.len());
        for (mi, msg) in messages.into_iter().enumerate() {
            let mut new_content = Vec::with_capacity(msg.content.len());
            for (bi, block) in msg.content.into_iter().enumerate() {
                match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } if !keep.contains(&(mi, bi)) && content.len() > 200 => {
                        let boundary = content.floor_char_boundary(100);
                        let summary = format!(
                            "[Tool result removed. Size: {} bytes. Preview: {}...]",
                            content.len(),
                            &content[..boundary]
                        );
                        new_content.push(ContentBlock::ToolResult {
                            tool_use_id,
                            content: summary,
                            is_error,
                        });
                    }
                    other => new_content.push(other),
                }
            }
            compacted.push(Message {
                role: msg.role,
                content: new_content,
            });
        }
        compacted
    }

    /// Aggressive variant of [`Self::emergency_compact`] used after a
    /// context-too-long API error has already fired. Differences:
    ///
    /// - ALL `ToolResult` blocks not in the keep set are replaced with a
    ///   short stub (no 200-byte size threshold).
    /// - `Thinking` blocks are stripped entirely from non-keep messages.
    /// - `Text` blocks longer than 400 chars are truncated in non-keep
    ///   messages.
    ///
    /// Like `emergency_compact`, this is position-independent: `keep_recent`
    /// counts tool-result occurrences, not messages. The "non-keep messages"
    /// are messages whose tool results would be shrunk — text/thinking in
    /// those messages also gets pruned.
    pub fn hard_emergency_compact(
        messages: Vec<Message>,
        keep_recent_tool_results: usize,
    ) -> Vec<Message> {
        let locations = Self::locate_large_tool_results(&messages, 0);
        let keep = Self::keep_set(&locations, keep_recent_tool_results);

        // Messages whose tool results will be kept verbatim — preserve their
        // text/thinking too as part of the working memory.
        let keep_msg_indices: std::collections::HashSet<usize> =
            keep.iter().map(|(mi, _)| *mi).collect();

        let mut compacted = Vec::with_capacity(messages.len());
        for (mi, msg) in messages.into_iter().enumerate() {
            let preserve_verbatim = keep_msg_indices.contains(&mi);
            let mut new_content = Vec::with_capacity(msg.content.len());
            for (bi, block) in msg.content.into_iter().enumerate() {
                match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        content,
                    } => {
                        if keep.contains(&(mi, bi)) {
                            new_content.push(ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            });
                        } else {
                            new_content.push(ContentBlock::ToolResult {
                                tool_use_id,
                                content: "[tool result elided for context pressure]"
                                    .to_string(),
                                is_error,
                            });
                        }
                    }
                    ContentBlock::Thinking { thinking, reasoning_details } => {
                        // Keep thinking only in preserved-verbatim messages.
                        if preserve_verbatim {
                            new_content.push(ContentBlock::Thinking {
                                thinking,
                                reasoning_details,
                            });
                        }
                    }
                    ContentBlock::Text { text }
                        if !preserve_verbatim && text.len() > 400 =>
                    {
                        let boundary = text.floor_char_boundary(400);
                        new_content.push(ContentBlock::Text {
                            text: format!("{}… [truncated]", &text[..boundary]),
                        });
                    }
                    other => new_content.push(other),
                }
            }
            compacted.push(Message {
                role: msg.role,
                content: new_content,
            });
        }
        compacted
    }
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

    #[test]
    fn test_dynamic_thresholds_small_window() {
        let budget = ContextBudget::with_window_size(32_000);
        assert!((budget.warning_threshold - 0.55).abs() < 1e-10);
        assert!((budget.compact_threshold - 0.65).abs() < 1e-10);
        assert!((budget.hard_limit - 0.85).abs() < 1e-10);
    }

    #[test]
    fn test_dynamic_thresholds_medium_window() {
        let budget = ContextBudget::with_window_size(100_000);
        assert!((budget.warning_threshold - 0.60).abs() < 1e-10);
        assert!((budget.compact_threshold - 0.70).abs() < 1e-10);
        assert!((budget.hard_limit - 0.90).abs() < 1e-10);
    }

    #[test]
    fn test_dynamic_thresholds_large_window() {
        let budget = ContextBudget::with_window_size(200_000);
        assert!((budget.warning_threshold - 0.70).abs() < 1e-10);
        assert!((budget.compact_threshold - 0.75).abs() < 1e-10);
        assert!((budget.hard_limit - 0.95).abs() < 1e-10);
    }

    #[test]
    fn test_dynamic_thresholds_boundary_64k() {
        // Exactly 64k should be medium tier
        let budget = ContextBudget::with_window_size(64_000);
        assert!((budget.warning_threshold - 0.60).abs() < 1e-10);
        // Just below 64k should be small tier
        let budget_small = ContextBudget::with_window_size(63_999);
        assert!((budget_small.warning_threshold - 0.55).abs() < 1e-10);
    }

    #[test]
    fn test_dynamic_thresholds_boundary_128k() {
        // Exactly 128k should be large tier
        let budget = ContextBudget::with_window_size(128_000);
        assert!((budget.warning_threshold - 0.70).abs() < 1e-10);
        // Just below 128k should be medium tier
        let budget_med = ContextBudget::with_window_size(127_999);
        assert!((budget_med.warning_threshold - 0.60).abs() < 1e-10);
    }

    /// Helper: build N user messages, each with a text block of `chars_per_msg` chars.
    fn user_messages(n: usize, chars_per_msg: usize) -> Vec<Message> {
        (0..n)
            .map(|_| Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "x".repeat(chars_per_msg),
                }],
            })
            .collect()
    }

    /// Helper: build a message with one ToolResult block of `chars` chars.
    fn tool_result_message(chars: usize, id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: "y".repeat(chars),
                is_error: false,
            }],
        }
    }

    #[test]
    fn test_check_pressure_accounts_for_overhead() {
        // 32k window, small-tier thresholds: compact at 0.65 → 20_800 tokens
        // With 10k overhead, compaction should trigger at ~10_800 message tokens,
        // not ~20_800 as it would without overhead accounting.
        let budget = ContextBudget::with_window_size(32_000).with_overhead(10_000);

        // 40_000 chars / 4 = 10_000 message tokens; + 10_000 overhead = 20_000 effective
        // 20_000 / 32_000 = 0.625 → below compact_threshold (0.65), still Warning range.
        let msgs = user_messages(1, 40_000);
        match budget.check_pressure(&msgs) {
            ContextPressureAction::Warning => {}
            other => panic!("expected Warning at 62.5%, got {:?}", other),
        }

        // 48_000 chars / 4 = 12_000 msg tokens; + 10_000 overhead = 22_000 effective
        // 22_000 / 32_000 = 0.6875 → above compact_threshold (0.65) → EmergencyCompaction
        let msgs = user_messages(1, 48_000);
        match budget.check_pressure(&msgs) {
            ContextPressureAction::EmergencyCompaction => {}
            other => panic!("expected EmergencyCompaction at 68.75%, got {:?}", other),
        }

        // Sanity: the same message volume with NO overhead is at 37.5% — well
        // below the 55% warning threshold — so the old logic would have called
        // this Ok and done nothing, letting the agent blow past real capacity.
        let budget_no_overhead = ContextBudget::with_window_size(32_000);
        match budget_no_overhead.check_pressure(&msgs) {
            ContextPressureAction::Ok => {}
            other => panic!(
                "without overhead, 12k msg tokens / 32k = 37.5% should be Ok, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn test_effective_tokens_sums_messages_and_overhead() {
        let budget = ContextBudget::with_window_size(100_000).with_overhead(5_000);
        // 400 chars / 4 = 100 message tokens
        let msgs = user_messages(1, 400);
        assert_eq!(budget.estimate_tokens(&msgs), 100);
        assert_eq!(budget.effective_tokens(&msgs), 5_100);
    }

    #[test]
    fn test_emergency_compact_shrinks_recent_position_tool_results() {
        // Regression for the smoke-stress-compaction bug: a chatty
        // file-reading workload puts big tool results in RECENT positions.
        // The old message-position-based "keep last N messages" rule would
        // protect them and compaction would reduce zero tokens. The new
        // occurrence-based rule (keep last N TOOL RESULTS) should shrink
        // earlier-occurrence tool results regardless of where they sit.
        //
        // Build a realistic 9-message layout:
        //   0: user task
        //   1: assistant (text + tool_use for read_file A)
        //   2: user (tool_result with 10KB content)  ← old
        //   3: assistant (text + tool_use for read_file B)
        //   4: user (tool_result with 10KB content)  ← older-but-"recent" position
        //   5: assistant (text + tool_use for read_file C)
        //   6: user (tool_result with 10KB content)  ← most-recent position
        //   7: assistant (text)
        //   8: user (text)
        let mut messages = Vec::new();
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "Summarize all docs".to_string(),
            }],
        });
        for i in 0..3 {
            messages.push(Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: format!("Reading file {}", i),
                    },
                    ContentBlock::ToolUse {
                        id: format!("tu-{}", i),
                        name: "read_file".to_string(),
                        input: serde_json::json!({"path": format!("docs/{}.md", i)}),
                    },
                ],
            });
            messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: format!("tu-{}", i),
                    content: "x".repeat(10_000),
                    is_error: false,
                }],
            });
        }
        messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "Now I'll write the summary".to_string(),
            }],
        });

        let budget = ContextBudget::with_window_size(32_000);
        let before = budget.estimate_tokens(&messages);

        // keep_recent_tool_results=2 — should shrink the FIRST of 3 tool
        // results (~10KB) while preserving the last 2 verbatim.
        let after = ContextBudget::emergency_compact(messages.clone(), 2);
        let after_tokens = budget.estimate_tokens(&after);
        let delta = before.saturating_sub(after_tokens);

        assert_eq!(after.len(), messages.len(), "message count preserved");
        assert!(
            delta >= 2000,
            "compaction should shrink at least one 10KB tool result \
             (~2500 tokens reduction expected), got delta={} (before={}, after={})",
            delta,
            before,
            after_tokens,
        );

        // keep_recent_tool_results=1 — should shrink 2 of 3 tool results.
        let aggressive = ContextBudget::emergency_compact(messages.clone(), 1);
        let aggressive_tokens = budget.estimate_tokens(&aggressive);
        assert!(
            aggressive_tokens < after_tokens,
            "keep_recent=1 should reduce more than keep_recent=2: {} vs {}",
            aggressive_tokens,
            after_tokens,
        );

        // keep_recent_tool_results=0 — should shrink ALL tool results.
        let maximum = ContextBudget::emergency_compact(messages.clone(), 0);
        let maximum_tokens = budget.estimate_tokens(&maximum);
        assert!(
            maximum_tokens < aggressive_tokens,
            "keep_recent=0 should reduce more than keep_recent=1: {} vs {}",
            maximum_tokens,
            aggressive_tokens,
        );
    }

    #[test]
    fn test_hard_emergency_compact_reduces_more_than_soft() {
        // 8 older messages, each with a 1000-char tool result, plus 3 recent messages.
        let mut messages: Vec<Message> = (0..8)
            .map(|i| tool_result_message(1000, &format!("tu-{}", i)))
            .collect();
        messages.extend(user_messages(3, 100));

        let budget = ContextBudget::with_window_size(32_000);
        let before_tokens = budget.estimate_tokens(&messages);

        // Soft variant: keeps last 5, so only 6 older messages get their
        // tool results shrunk (to ~150 chars each).
        let soft = ContextBudget::emergency_compact(messages.clone(), 5);
        let soft_tokens = budget.estimate_tokens(&soft);
        assert_eq!(soft.len(), messages.len(), "message count preserved");
        assert!(
            soft_tokens < before_tokens,
            "soft compact should reduce tokens: {} → {}",
            before_tokens,
            soft_tokens
        );

        // Hard variant with keep_recent=2: more messages get compacted AND
        // the replacement stub is smaller ("[tool result elided...]").
        let hard = ContextBudget::hard_emergency_compact(messages.clone(), 2);
        let hard_tokens = budget.estimate_tokens(&hard);
        assert_eq!(hard.len(), messages.len(), "message count preserved");
        assert!(
            hard_tokens < soft_tokens,
            "hard compact should reduce more than soft: soft={}, hard={}",
            soft_tokens,
            hard_tokens
        );
    }

    #[test]
    fn test_hard_emergency_compact_strips_thinking_and_truncates_text() {
        let mut messages = vec![
            // Older message with thinking + long text
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "a".repeat(2000),
                        reasoning_details: None,
                    },
                    ContentBlock::Text {
                        text: "b".repeat(2000),
                    },
                ],
            },
        ];
        // Keep 1 recent message verbatim so the older message actually gets compacted.
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text {
                text: "recent".to_string(),
            }],
        });

        let hard = ContextBudget::hard_emergency_compact(messages, 1);

        // Older message: thinking dropped, text truncated
        let older = &hard[0];
        let has_thinking = older
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::Thinking { .. }));
        assert!(!has_thinking, "hard compact should strip thinking blocks");

        let text_block = older
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("text block should still be present");
        assert!(
            text_block.len() < 2000,
            "long text should be truncated, got {} chars",
            text_block.len()
        );
        assert!(text_block.contains("[truncated]"));

        // Recent message untouched
        assert_eq!(hard[1].content.len(), 1);
    }

    #[test]
    fn test_estimate_agent_overhead_accounts_for_all_sources() {
        let system_prompt = "s".repeat(4000); // 4000 chars / 4 = 1000 tokens
        let tool_defs = vec![ToolDefinition {
            name: "bash".to_string(),
            description: "d".repeat(200),
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let max_tokens = 2048_u32;

        let overhead = estimate_agent_overhead(&system_prompt, &tool_defs, max_tokens, 4.0);

        // system: 4000 chars, tool name: 4, tool desc: 200, schema: ~40
        // static_tokens ≈ (4000 + 4 + 200 + 40) / 4 ≈ 1061
        // total ≈ 1061 + 2048 = 3109
        assert!(
            overhead >= 2048 + 1000 && overhead <= 2048 + 1100,
            "overhead should roughly sum system + tools + max_tokens, got {}",
            overhead
        );
    }
}
