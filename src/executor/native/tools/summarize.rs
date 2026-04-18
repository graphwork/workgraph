//! Summarize tool: recursive map-reduce summarization for arbitrarily-large text.
//!
//! Given a source (file path or inline text), produces a single focused
//! summary via the following algorithm:
//!
//! 1. Refuse if input exceeds `max_input_bytes` (default 1 MiB).
//! 2. If the input fits in a single model call (~40% of context window),
//!    summarize directly and return.
//! 3. Otherwise chunk the input on paragraph boundaries when possible,
//!    summarize each chunk independently with an `instruction`-aware
//!    focus prompt, concatenate the chunk summaries, and recurse.
//! 4. Terminate at `MAX_RECURSION_DEPTH` or a single-chunk base case.
//!
//! This is the cornerstone primitive from the reliability action plan
//! (L2): it lets agents reduce arbitrarily-large content hierarchically
//! without ever loading more than a single chunk into context at once.
//!
//! Unlike `delegate` (which runs a general sub-agent with tools),
//! `summarize` issues direct text-in/text-out LLM calls — no tool loop,
//! no recursion into other tools. That makes it cheap and predictable.

use std::path::{Path, PathBuf};
use std::pin::Pin;

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolOutput, truncate_tool_output};
use crate::executor::native::client::{
    ContentBlock, Message, MessagesRequest, Role, ToolDefinition,
};
use crate::executor::native::provider::Provider;

/// Default hard ceiling on input size (bytes). Prevents accidental
/// whole-codebase summarizations that would take forever.
const DEFAULT_MAX_INPUT_BYTES: usize = 1_000_000;

/// Max output tokens for each summarization LLM call.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 1024;

/// Tool-level output truncation (the tool result itself).
const MAX_SUMMARIZE_OUTPUT_CHARS: usize = 8_000;

/// Fraction of context window to use per chunk. Leaves headroom for
/// system prompt + instruction + reasoning + completion.
const CHUNK_CONTEXT_FRACTION: f64 = 0.40;

/// Chars-per-token estimate for chunk sizing.
const CHARS_PER_TOKEN: f64 = 4.0;

/// Maximum recursion depth to prevent runaway trees if chunks don't shrink.
const MAX_RECURSION_DEPTH: usize = 8;

/// System prompt for summarization LLM calls. Intentionally terse — the
/// focus instruction carries the task-specific guidance.
const SUMMARIZE_SYSTEM_PROMPT: &str = "\
You are a text summarization agent. Given a chunk of text and a focus \
instruction, produce a concise summary that preserves the details the \
instruction asks for. Return only the summary text — no preamble, no \
commentary, no meta-discussion.";

/// The summarize tool.
pub struct SummarizeTool {
    workgraph_dir: PathBuf,
    /// Model override for summarization calls. Empty = use `WG_MODEL` env
    /// var (set by the coordinator at spawn time).
    model: String,
}

impl SummarizeTool {
    pub fn new(workgraph_dir: PathBuf, model: String) -> Self {
        Self {
            workgraph_dir,
            model,
        }
    }
}

#[async_trait]
impl Tool for SummarizeTool {
    fn name(&self) -> &str {
        "summarize"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "summarize".to_string(),
            description: "Recursively summarize a large text source via map-reduce. \
                Reads from a file path or inline text, chunks it to fit the model's \
                context window, summarizes each chunk independently with your focus \
                instruction, then merges — recursing if the merged summaries are still \
                too large. Use this when a source is too big to read directly (large \
                files, long logs, big tool outputs). Each level runs as direct text \
                LLM calls with no tool loop, so it's cheap and predictable."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "Either a file path (relative or absolute) or inline text. If the string resolves to an existing file on disk, the file is loaded; otherwise the string itself is treated as the input text."
                    },
                    "instruction": {
                        "type": "string",
                        "description": "Focus instruction — what to preserve in the summary. E.g. 'extract public function signatures', 'focus on error handling', 'list the section headings'. Defaults to a generic 'summarize the text' if not provided."
                    },
                    "max_input_bytes": {
                        "type": "integer",
                        "description": "Hard ceiling on input size in bytes (default 1000000 = 1 MB). Sources larger than this are rejected to prevent runaway cost."
                    }
                },
                "required": ["source"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        // 1. Parse input
        let source = match input.get("source").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s,
            _ => {
                return ToolOutput::error(
                    "Missing or empty required parameter: source".to_string(),
                );
            }
        };
        let instruction = input
            .get("instruction")
            .and_then(|v| v.as_str())
            .unwrap_or("Summarize the text.");
        let max_input_bytes = input
            .get("max_input_bytes")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_INPUT_BYTES);

        // 2. Load content — file path or inline
        let content = {
            let as_path = Path::new(source);
            if as_path.exists() && as_path.is_file() {
                match std::fs::read_to_string(as_path) {
                    Ok(c) => c,
                    Err(e) => {
                        return ToolOutput::error(format!(
                            "Failed to read source file '{}': {}",
                            source, e
                        ));
                    }
                }
            } else {
                source.to_string()
            }
        };

        if content.len() > max_input_bytes {
            return ToolOutput::error(format!(
                "Input exceeds max_input_bytes: {} > {} (raise max_input_bytes to allow larger inputs)",
                content.len(),
                max_input_bytes
            ));
        }

        // 3. Create provider
        let model = if !self.model.is_empty() {
            self.model.clone()
        } else {
            std::env::var("WG_MODEL")
                .ok()
                .filter(|m| !m.is_empty())
                .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string())
        };

        let provider =
            match crate::executor::native::provider::create_provider(&self.workgraph_dir, &model) {
                Ok(p) => p,
                Err(e) => {
                    return ToolOutput::error(format!(
                        "Failed to create provider for summarize: {}",
                        e
                    ));
                }
            };

        eprintln!(
            "\x1b[2m[summarize] starting: model={}, input_bytes={}\x1b[0m",
            model,
            content.len(),
        );

        // 4. Recursive summarize
        match recursive_summarize(provider.as_ref(), &content, instruction, 0).await {
            Ok(summary) => {
                let truncated = truncate_tool_output(&summary, MAX_SUMMARIZE_OUTPUT_CHARS);
                ToolOutput::success(truncated)
            }
            Err(e) => ToolOutput::error(format!("Summarize failed: {}", e)),
        }
    }
}

/// Estimate how many chars of input fit in one summarization LLM call,
/// leaving headroom for system prompt, instruction, reasoning, and output.
pub(crate) fn chunk_size_chars(window_size: usize) -> usize {
    ((window_size as f64) * CHUNK_CONTEXT_FRACTION * CHARS_PER_TOKEN) as usize
}

/// Split `text` into chunks of approximately `chunk_chars` bytes each.
/// Prefers to break on paragraph boundaries (`\n\n`) within the final 20%
/// of each chunk. Falls back to char-boundary truncation otherwise.
pub(crate) fn chunk_text(text: &str, chunk_chars: usize) -> Vec<String> {
    if text.len() <= chunk_chars {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut pos = 0;

    while pos < text.len() {
        let target_end = (pos + chunk_chars).min(text.len());
        let end = text.floor_char_boundary(target_end);

        let break_pt = if end == text.len() {
            end
        } else {
            // Prefer a paragraph boundary in the last 20% of the chunk.
            let min_break = pos + (chunk_chars * 4 / 5);
            if min_break < end {
                text[min_break..end]
                    .rfind("\n\n")
                    .map(|i| min_break + i + 2)
                    .unwrap_or(end)
            } else {
                end
            }
        };

        chunks.push(text[pos..break_pt].to_string());
        pos = break_pt;
    }

    chunks
}

/// Recursively summarize text via map-reduce.
///
/// `depth` is the recursion level; bail at `MAX_RECURSION_DEPTH` to
/// prevent runaway trees when summaries aren't shrinking fast enough.
///
/// This is exposed as `pub(crate)` because L3 compaction (in `agent.rs`)
/// calls it directly on a serialized representation of the agent's own
/// message history when the escalation ladder saturates.
pub(crate) fn recursive_summarize<'a>(
    provider: &'a dyn Provider,
    text: &'a str,
    instruction: &'a str,
    depth: usize,
) -> Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    recursive_summarize_cancellable(provider, text, instruction, depth, None)
}

/// Cancellable variant: a clone of the outer agent loop's `CancelToken`
/// can be threaded in. Between each chunk (and before starting the
/// whole call), we check `cancel.is_cooperative()` and bail with an
/// error if the user has hit Ctrl-C. This stops wasted work after the
/// current chunk completes, without needing to plumb cancellation
/// into every individual `provider.send()` call. If the outer cancel
/// is `None`, behavior is identical to the non-cancellable variant.
pub(crate) fn recursive_summarize_cancellable<'a>(
    provider: &'a dyn Provider,
    text: &'a str,
    instruction: &'a str,
    depth: usize,
    cancel: Option<crate::executor::native::cancel::CancelToken>,
) -> Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>> {
    Box::pin(async move {
        if let Some(ref c) = cancel
            && c.is_cooperative()
        {
            return Err("summarize cancelled by user".to_string());
        }
        if depth >= MAX_RECURSION_DEPTH {
            return Err(format!(
                "Max recursion depth ({}) exceeded — summaries are not shrinking. \
                 Input may be pathologically long or chunk_size too large.",
                MAX_RECURSION_DEPTH
            ));
        }

        let window_size = provider.context_window();
        let chunk_chars = chunk_size_chars(window_size);

        // Base case: fits in one call.
        if text.len() <= chunk_chars {
            let started = std::time::Instant::now();
            let result = summarize_chunk(provider, text, instruction).await;
            let elapsed = started.elapsed();
            if let Ok(ref summary) = result {
                log_summarize_call(depth, text.len(), summary.len(), elapsed);
            }
            return result;
        }

        // Recursive case: chunk, map, reduce.
        let chunks = chunk_text(text, chunk_chars);
        eprintln!(
            "\x1b[2m[summarize] depth={}: chunking {} bytes into {} parts (chunk_chars={})\x1b[0m",
            depth,
            text.len(),
            chunks.len(),
            chunk_chars
        );

        // Map: summarize each chunk independently. Check cancel
        // between chunks so a Ctrl-C bails out after at most the
        // currently-in-flight chunk completes.
        let mut chunk_summaries = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.iter().enumerate() {
            if let Some(ref c) = cancel
                && c.is_cooperative()
            {
                return Err(format!(
                    "summarize cancelled by user after chunk {}/{}",
                    i,
                    chunks.len()
                ));
            }
            let chunk_instruction = format!(
                "This is part {} of {} of a larger document. {}",
                i + 1,
                chunks.len(),
                instruction
            );
            let started = std::time::Instant::now();
            let summary = summarize_chunk(provider, chunk, &chunk_instruction).await?;
            let elapsed = started.elapsed();
            log_summarize_chunk(depth, i + 1, chunks.len(), chunk.len(), summary.len(), elapsed);
            chunk_summaries.push(summary);
        }

        // Reduce: merge chunk summaries.
        let merged = chunk_summaries.join("\n\n---\n\n");

        if merged.len() <= chunk_chars {
            // Final merge pass.
            let merge_instruction = format!(
                "These are {} partial summaries of a larger document. Merge them into \
                 one coherent, non-redundant summary. {}",
                chunks.len(),
                instruction
            );
            let started = std::time::Instant::now();
            let result = summarize_chunk(provider, &merged, &merge_instruction).await;
            let elapsed = started.elapsed();
            if let Ok(ref summary) = result {
                eprintln!(
                    "\x1b[2m[summarize] depth={} merge: {} bytes → {} bytes in {:.1}s ({})\x1b[0m",
                    depth,
                    merged.len(),
                    summary.len(),
                    elapsed.as_secs_f64(),
                    throughput_label(summary.len(), elapsed),
                );
            }
            return result;
        }

        // Merged summaries still too large — recurse.
        eprintln!(
            "\x1b[2m[summarize] depth={}: merged summaries still too large ({} bytes), recursing\x1b[0m",
            depth,
            merged.len()
        );
        recursive_summarize_cancellable(provider, &merged, instruction, depth + 1, cancel).await
    })
}

/// Format a single-shot summarization telemetry line. Dim-styled so the
/// user can visually tune it out when the agent is working hard but it
/// stays visible for when they want to check on progress.
fn log_summarize_call(depth: usize, in_bytes: usize, out_bytes: usize, elapsed: std::time::Duration) {
    eprintln!(
        "\x1b[2m[summarize] depth={}: {} bytes → {} bytes in {:.1}s ({})\x1b[0m",
        depth,
        in_bytes,
        out_bytes,
        elapsed.as_secs_f64(),
        throughput_label(out_bytes, elapsed),
    );
}

fn log_summarize_chunk(
    depth: usize,
    i: usize,
    total: usize,
    in_bytes: usize,
    out_bytes: usize,
    elapsed: std::time::Duration,
) {
    eprintln!(
        "\x1b[2m[summarize] depth={} chunk {}/{}: {} bytes → {} bytes in {:.1}s ({})\x1b[0m",
        depth,
        i,
        total,
        in_bytes,
        out_bytes,
        elapsed.as_secs_f64(),
        throughput_label(out_bytes, elapsed),
    );
}

/// Convert (output bytes, elapsed) into a compact throughput label.
/// We show output-tok/s (approx = output_bytes / 4 / seconds) because
/// that's the metric the user cares about — how fast is the summarizer
/// producing text? Input is effectively free (prefill).
fn throughput_label(out_bytes: usize, elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs_f64();
    if secs < 0.001 {
        return "instant".to_string();
    }
    let tok_per_sec = (out_bytes as f64 / 4.0) / secs;
    format!("≈{:.0} tok/s out", tok_per_sec)
}

/// Issue a single summarization LLM call. Text-in/text-out, no tools.
/// Wall-clock timeout for a single summarize LLM call. Bounds the
/// worst-case time even if the provider hangs. Claude-code-ts uses
/// 120s for its non-streaming fallback; we go slightly higher (180s)
/// because summarize can legitimately process large chunks and the
/// cost of a premature abort (lost summary, repeat work) is higher
/// than a slow success. Override via env WG_SUMMARIZE_TIMEOUT_SECS.
const SUMMARIZE_CALL_TIMEOUT_SECS: u64 = 180;

async fn summarize_chunk(
    provider: &dyn Provider,
    text: &str,
    instruction: &str,
) -> Result<String, String> {
    let prompt = format!("Instruction: {}\n\n---\n\n{}", instruction, text);
    let request = MessagesRequest {
        model: provider.model().to_string(),
        max_tokens: DEFAULT_MAX_OUTPUT_TOKENS,
        system: Some(SUMMARIZE_SYSTEM_PROMPT.to_string()),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }],
        tools: vec![],
        stream: false,
    };

    let timeout_secs = std::env::var("WG_SUMMARIZE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(SUMMARIZE_CALL_TIMEOUT_SECS);
    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        provider.send(&request),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => return Err(format!("API error in summarize call: {}", e)),
        Err(_) => {
            return Err(format!(
                "summarize LLM call timed out after {}s (set WG_SUMMARIZE_TIMEOUT_SECS to raise)",
                timeout_secs
            ));
        }
    };

    let text: String = response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    if text.trim().is_empty() {
        return Err("Empty summary response from provider".to_string());
    }

    Ok(text)
}

/// Register the summarize tool in the given registry.
pub fn register_summarize_tool(
    registry: &mut super::ToolRegistry,
    workgraph_dir: PathBuf,
    model: String,
) {
    registry.register(Box::new(SummarizeTool::new(workgraph_dir, model)));
}

// ───────────────────────────────────────────────────────────────────────
// L3: Summarize-based compaction of an agent's own message history.
// Used by `agent.rs` when the standard emergency_compact /
// hard_emergency_compact ladder has saturated (repeated no-op fires at
// the compact threshold).
// ───────────────────────────────────────────────────────────────────────

/// Number of recent messages to preserve verbatim when summarizing an
/// agent's history. The summary captures older turns; these stay intact
/// so the model keeps immediate working memory.
const L3_KEEP_RECENT_MESSAGES: usize = 2;

/// Instruction for the L3 history-summarization call. Designed to
/// Nine-section structured summary prompt. Ported from Claude Code
/// (`services/compact/prompt.ts` in the TypeScript reverse-engineering
/// at ~/executors/claude-code-ts). Each section forces the summarizer
/// to preserve load-bearing content that a generic "summarize this"
/// prompt routinely drops:
///
/// 1. Primary Request and Intent — what the user actually wants.
/// 2. Key Technical Concepts — vocabulary established in the session.
/// 3. Files and Code Sections — which files were touched and why.
/// 4. Errors and Fixes — so the agent doesn't repeat its own mistakes.
/// 5. Problem Solving — the paths tried and discarded.
/// 6. All user messages (verbatim) — anchors the session to human intent.
/// 7. Pending Tasks — what's still open.
/// 8. Current Work — where we were when compaction fired.
/// 9. Optional Next Step — what to do right after resuming.
///
/// Ground-truth evidence this prompt works: this very Claude Code
/// session was compacted with this exact structure and retained enough
/// to keep the design conversation fully coherent through the
/// compaction boundary.
const HISTORY_SUMMARY_INSTRUCTION: &str = "\
Your task is to create a detailed summary of the conversation so far, paying \
close attention to the user's explicit requests and your previous actions. \
This summary should be thorough in capturing technical details, code patterns, \
and architectural decisions that would be essential for continuing development \
work without losing context.\n\
\n\
Your summary should include the following sections:\n\
\n\
1. Primary Request and Intent: Capture all of the user's explicit requests and \
intents in detail.\n\
2. Key Technical Concepts: List all important technical concepts, technologies, \
and frameworks discussed.\n\
3. Files and Code Sections: Enumerate specific files and code sections examined, \
modified, or created. Pay special attention to the most recent messages and \
include full code snippets where applicable, and a summary of why this file \
read or edit is important.\n\
4. Errors and Fixes: List all errors that you ran into, and how you fixed them. \
Pay special attention to specific user feedback that you received.\n\
5. Problem Solving: Document problems solved and any ongoing troubleshooting \
efforts.\n\
6. All user messages: List ALL user messages that are not tool results. These are \
critical for understanding the users' feedback and changing intent.\n\
7. Pending Tasks: Outline any pending tasks that you have explicitly been asked \
to work on.\n\
8. Current Work: Describe in detail precisely what was being worked on \
immediately before this summary request. Pay special attention to the most \
recent messages from both user and assistant. Include file names and code \
snippets where applicable.\n\
9. Optional Next Step: List the next step that you will take that is related to \
the most recent work you were doing. IMPORTANT: ensure that this step is DIRECTLY \
in line with the user's explicit requests, and the task you were working on \
immediately before this summary request. If your last task was concluded, then \
only list next steps if they are explicitly in line with the users request. Do \
not start on tangential requests without confirming with the user first.\n\
\n\
Output the summary in clear, skimmable prose with explicit section headers. \
Preserve exact file paths, function names, error messages, and user-quoted \
phrases verbatim — paraphrasing these is worse than omitting them.";

/// Per-block microcompact instruction. Much narrower than the
/// history-wide one because it operates on a single tool output /
/// assistant turn rather than the whole conversation. The agent
/// typically doesn't need the verbatim content of an old tool result
/// — it needs enough signal to know whether to re-query or move on.
const BLOCK_SUMMARY_INSTRUCTION: &str = "\
Summarize this block of content so a future version of the agent \
reading it knows: what was found, what decisions it supports, which \
specific filenames / URLs / identifiers were mentioned, and which \
questions it settled. Preserve exact names verbatim. Drop verbatim \
body text, pleasantries, progress narration, and redundant framing. \
Target a summary under 200 words unless the content genuinely \
requires more.";

/// Serialize a `Message` to a compact text representation suitable for
/// inclusion in a summarization prompt.
fn serialize_message_for_summary(msg: &Message) -> String {
    let role = match msg.role {
        Role::User => "USER",
        Role::Assistant => "ASSISTANT",
    };
    let mut parts = Vec::new();
    for block in &msg.content {
        match block {
            ContentBlock::Text { text } => {
                parts.push(text.clone());
            }
            ContentBlock::Thinking { thinking, .. } => {
                parts.push(format!("[thinking] {}", thinking));
            }
            ContentBlock::ToolUse { name, input, .. } => {
                parts.push(format!("[tool_use {}] {}", name, input));
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                let prefix = if *is_error {
                    "[tool_result ERROR]"
                } else {
                    "[tool_result]"
                };
                parts.push(format!("{} {}", prefix, content));
            }
        }
    }
    format!("{}: {}", role, parts.join("\n"))
}

/// Compact an agent's message history via recursive summarization.
///
/// This is the L3 tier of the compaction escalation ladder. When
/// `emergency_compact` and `hard_emergency_compact` can no longer reduce
/// message tokens (the accumulation is in Text/Thinking/ToolUse content
/// the model itself produced), this function:
///
/// 1. Splits `messages` into `older` (everything except the last
///    `L3_KEEP_RECENT_MESSAGES`) and `recent` (the tail, kept verbatim).
/// 2. Serializes `older` to a text transcript.
/// 3. Invokes `recursive_summarize` to reduce the transcript to a
///    bounded-size summary (recursing as needed for very long histories).
/// 4. Returns a new message vec:
///    `[User("PRIOR CONVERSATION SUMMARY: <summary>"), recent...]`
///
/// Like the other compaction functions this preserves tool_use/tool_result
/// pairing in `recent`, and message count is replaced (not preserved) —
/// this is a more aggressive intervention that explicitly drops the
/// older-turn structure in exchange for a bounded-size replacement.
///
/// On failure (provider errors, empty summary) the original messages are
/// returned unchanged — compaction is best-effort, never a blocker.
pub async fn summarize_history_for_compaction(
    provider: &dyn Provider,
    messages: Vec<Message>,
) -> Vec<Message> {
    summarize_history_for_compaction_cancellable(provider, messages, None).await
}

/// Cancellable variant. See `recursive_summarize_cancellable`. Passes
/// the optional `CancelToken` down into the inner summarize chain
/// so a Ctrl-C during compaction aborts it after the currently-in-
/// flight chunk instead of waiting for the whole history summary to
/// complete.
pub async fn summarize_history_for_compaction_cancellable(
    provider: &dyn Provider,
    messages: Vec<Message>,
    cancel: Option<crate::executor::native::cancel::CancelToken>,
) -> Vec<Message> {
    if messages.len() <= L3_KEEP_RECENT_MESSAGES + 1 {
        // Not enough history to bother summarizing — the standard
        // compact already handles small vecs.
        return messages;
    }

    let split = messages.len() - L3_KEEP_RECENT_MESSAGES;
    let older = &messages[..split];
    let recent = &messages[split..];

    let transcript: String = older
        .iter()
        .map(serialize_message_for_summary)
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");

    eprintln!(
        "\x1b[2m[summarize-history] compacting {} older messages ({} bytes transcript), keeping {} recent\x1b[0m",
        older.len(),
        transcript.len(),
        recent.len()
    );

    let summary = match recursive_summarize_cancellable(
        provider,
        &transcript,
        HISTORY_SUMMARY_INSTRUCTION,
        0,
        cancel,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "\x1b[33m[summarize-history] recursive_summarize failed: {} — returning messages unchanged\x1b[0m",
                e
            );
            return messages;
        }
    };

    if summary.trim().is_empty() {
        eprintln!(
            "\x1b[33m[summarize-history] empty summary — returning messages unchanged\x1b[0m"
        );
        return messages;
    }

    eprintln!(
        "\x1b[2m[summarize-history] summary produced: {} bytes (from {} bytes transcript)\x1b[0m",
        summary.len(),
        transcript.len()
    );

    // Build the new message vec: summary as a user-role context message,
    // followed by the preserved recent messages verbatim.
    let mut compacted = Vec::with_capacity(recent.len() + 1);
    compacted.push(Message {
        role: Role::User,
        content: vec![ContentBlock::Text {
            text: format!(
                "PRIOR CONVERSATION SUMMARY (older turns compacted to reduce context pressure):\n\n{}",
                summary
            ),
        }],
    });
    compacted.extend_from_slice(recent);
    compacted
}

/// Minimum block size (in bytes of content) considered for
/// microcompaction. Blocks smaller than this are left alone; the
/// overhead of an LLM call isn't worth it. 2KB matches one short
/// web_fetch preview or read_file output — the typical chatty-agent
/// tool result.
pub const MICROCOMPACT_MIN_BLOCK_BYTES: usize = 2_048;

/// Number of messages at the tail to keep verbatim during
/// microcompaction. Protects the agent's active working memory.
pub const MICROCOMPACT_KEEP_RECENT_MESSAGES: usize = 4;

/// Microcompact a single message vec by finding the oldest large
/// content block outside the recent-keep tail and replacing it with
/// a short LLM-generated summary.
///
/// Design: unlike the post-turn emergency compaction ladder, this is
/// intended to run at **every** turn boundary whenever context pressure
/// exceeds a configurable soft threshold — so pressure never builds
/// up to the emergency point. One cheap LLM call per turn at most;
/// on turns where no block exceeds the threshold, it's a no-op.
///
/// Unlike `emergency_compact` (which only replaces ToolResult blocks
/// with preview stubs) this works on ANY block type — ToolResult,
/// Text, Thinking — because the narrative accumulation *is* the
/// pressure in chatty sessions, not just tool output. That's the bug
/// the attached trace in `docs/design/native-executor-run-loop.md`
/// demonstrates: L1 ran 8 times with zero delta because all the old
/// tool_results were under its 200B threshold, while the real pressure
/// sat in accumulated text/thinking.
///
/// On provider failure or empty summary, returns the input unchanged —
/// microcompaction is best-effort, never a blocker.
///
/// Returns `(messages, bytes_freed)`.
pub async fn microcompact_oldest_block(
    provider: &dyn Provider,
    messages: Vec<Message>,
    keep_recent_messages: usize,
    min_block_bytes: usize,
) -> (Vec<Message>, usize) {
    if messages.len() <= keep_recent_messages {
        return (messages, 0);
    }
    let last_compactable_idx = messages.len() - keep_recent_messages;

    // Walk oldest-first looking for the first block above threshold.
    let mut target: Option<(usize, usize, String, bool)> = None; // (msg_idx, block_idx, original, is_text_like)
    'outer: for (mi, msg) in messages.iter().enumerate() {
        if mi >= last_compactable_idx {
            break;
        }
        for (bi, block) in msg.content.iter().enumerate() {
            match block {
                ContentBlock::ToolResult { content, .. } if content.len() >= min_block_bytes => {
                    target = Some((mi, bi, content.clone(), false));
                    break 'outer;
                }
                ContentBlock::Text { text } if text.len() >= min_block_bytes => {
                    target = Some((mi, bi, text.clone(), true));
                    break 'outer;
                }
                ContentBlock::Thinking { thinking, .. }
                    if thinking.len() >= min_block_bytes =>
                {
                    target = Some((mi, bi, thinking.clone(), true));
                    break 'outer;
                }
                _ => {}
            }
        }
    }

    let (mi, bi, original, _is_text_like) = match target {
        Some(t) => t,
        None => return (messages, 0),
    };

    let orig_len = original.len();

    // Run the summary. On error, leave the messages unchanged.
    let summary = match summarize_chunk(provider, &original, BLOCK_SUMMARY_INSTRUCTION).await {
        Ok(s) if !s.trim().is_empty() => s,
        Ok(_) => return (messages, 0),
        Err(e) => {
            eprintln!(
                "[microcompact] provider error summarizing block ({} bytes): {} — leaving unchanged",
                orig_len, e
            );
            return (messages, 0);
        }
    };

    if summary.len() >= orig_len {
        // Summary didn't shrink — skip the swap to avoid growing context.
        return (messages, 0);
    }

    let replacement_text = format!(
        "[summarized by microcompact: {} B → {} B]\n\n{}",
        orig_len,
        summary.len(),
        summary
    );
    let bytes_freed = orig_len.saturating_sub(replacement_text.len());

    let mut out = messages;
    let msg = &mut out[mi];
    msg.content[bi] = match &msg.content[bi] {
        ContentBlock::ToolResult {
            tool_use_id,
            is_error,
            ..
        } => ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: replacement_text,
            is_error: *is_error,
        },
        ContentBlock::Text { .. } => ContentBlock::Text {
            text: replacement_text,
        },
        ContentBlock::Thinking {
            reasoning_details, ..
        } => ContentBlock::Thinking {
            thinking: replacement_text,
            reasoning_details: reasoning_details.clone(),
        },
        other => other.clone(),
    };

    (out, bytes_freed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_size_chars_scales_with_window() {
        // At 32k window: 32000 * 0.40 * 4.0 = 51200 chars
        let small = chunk_size_chars(32_000);
        // At 200k window: 200000 * 0.40 * 4.0 = 320000 chars
        let large = chunk_size_chars(200_000);
        assert!(large > small);
        assert_eq!(small, 51_200);
        assert_eq!(large, 320_000);
    }

    #[test]
    fn test_chunk_text_short_input_single_chunk() {
        let text = "hello world";
        let chunks = chunk_text(text, 100);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], text);
    }

    #[test]
    fn test_chunk_text_long_input_splits_on_paragraph() {
        let para1 = "A".repeat(400);
        let para2 = "B".repeat(400);
        let para3 = "C".repeat(400);
        let text = format!("{}\n\n{}\n\n{}", para1, para2, para3);
        // Chunk size 500 → should split roughly at paragraph boundaries.
        let chunks = chunk_text(&text, 500);
        assert!(
            chunks.len() >= 2,
            "expected multiple chunks, got {}",
            chunks.len()
        );

        // Concatenation should reconstruct the original (no chars lost).
        let recombined: String = chunks.join("");
        assert_eq!(recombined, text);
    }

    #[test]
    fn test_chunk_text_long_input_no_paragraphs_falls_back_to_char_boundary() {
        let text = "x".repeat(2500);
        let chunks = chunk_text(&text, 1000);
        assert!(chunks.len() >= 3);
        let recombined: String = chunks.join("");
        assert_eq!(recombined, text);
    }

    #[test]
    fn test_chunk_text_respects_char_boundaries() {
        // Text with multi-byte chars at chunk boundary positions
        let text = "héllo wörld ".repeat(100);
        let chunks = chunk_text(&text, 50);
        let recombined: String = chunks.join("");
        assert_eq!(recombined, text);
    }

    #[test]
    fn test_tool_definition_has_source_required() {
        let tool = SummarizeTool::new(PathBuf::from("/tmp"), String::new());
        let def = tool.definition();
        assert_eq!(def.name, "summarize");
        let schema = def.input_schema.as_object().unwrap();
        let required = schema.get("required").unwrap().as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("source")));
    }

    // ── microcompact tests ──────────────────────────────────────────

    use crate::executor::native::client::{MessagesResponse, StopReason, Usage};
    use async_trait::async_trait;

    /// Provider that returns a fixed summary string on every `send()`.
    /// Used to exercise microcompact deterministically without hitting
    /// a real LLM.
    struct StubSummarizer {
        summary: String,
    }

    #[async_trait]
    impl Provider for StubSummarizer {
        fn name(&self) -> &str {
            "stub"
        }
        fn model(&self) -> &str {
            "stub"
        }
        fn max_tokens(&self) -> u32 {
            256
        }
        fn context_window(&self) -> usize {
            32_000
        }
        async fn send(&self, _req: &MessagesRequest) -> anyhow::Result<MessagesResponse> {
            Ok(MessagesResponse {
                id: "stub".to_string(),
                content: vec![ContentBlock::Text {
                    text: self.summary.clone(),
                }],
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
            })
        }
        async fn send_streaming(
            &self,
            _req: &MessagesRequest,
            _on_text: &(dyn Fn(String) + Send + Sync),
        ) -> anyhow::Result<MessagesResponse> {
            unreachable!("microcompact uses send(), not send_streaming()")
        }
    }

    #[tokio::test]
    async fn microcompact_replaces_oldest_large_tool_result() {
        let big = "x".repeat(5_000);
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "start".into() }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: big.clone(),
                    is_error: false,
                }],
            },
            // Recent tail (keep verbatim)
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "ok".into() }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "next".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "sure".into() }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "go".into() }],
            },
        ];

        let provider = StubSummarizer {
            summary: "[summary: the tool returned x's]".to_string(),
        };
        let (out, freed) = microcompact_oldest_block(&provider, messages, 4, 2_048).await;
        assert!(freed > 0, "expected bytes freed");
        // The tool_result at index 2 should be the one that got replaced.
        match &out[2].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert!(content.contains("summarized by microcompact"));
                assert!(content.contains("the tool returned"));
                assert!(content.len() < 5_000, "replacement must be smaller");
            }
            other => panic!("expected ToolResult at [2][0], got {:?}", other),
        }
        // Tail must be untouched.
        match &out[4].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "next"),
            _ => panic!("tail corrupted"),
        }
    }

    #[tokio::test]
    async fn microcompact_noop_when_no_block_above_threshold() {
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "tiny".into() }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "short".into() }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "next".into() }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "ok".into() }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "go".into() }],
            },
        ];
        let provider = StubSummarizer {
            summary: "unused".to_string(),
        };
        let (_out, freed) = microcompact_oldest_block(&provider, messages, 4, 2_048).await;
        assert_eq!(freed, 0, "no block above threshold → no compaction");
    }

    #[tokio::test]
    async fn microcompact_protects_recent_tail() {
        let big = "z".repeat(5_000);
        // Only big block is in the recent-keep tail — should be left alone.
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "a".into() }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "b".into() }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "c".into() }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: big.clone(),
                }],
            },
        ];
        let provider = StubSummarizer {
            summary: "unused".to_string(),
        };
        let (out, freed) = microcompact_oldest_block(&provider, messages, 4, 2_048).await;
        assert_eq!(freed, 0, "tail-only big block → no compaction");
        // Block still there verbatim.
        match &out[3].content[0] {
            ContentBlock::Text { text } => assert_eq!(text.len(), 5_000),
            _ => panic!("unexpected"),
        }
    }

    #[tokio::test]
    async fn microcompact_skips_when_summary_bigger_than_original() {
        let small = "y".repeat(2_100); // just above threshold
        let messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: small }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "a".into() }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "b".into() }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: "c".into() }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "d".into() }],
            },
        ];
        let provider = StubSummarizer {
            summary: "x".repeat(10_000), // bigger than the original
        };
        let (_out, freed) = microcompact_oldest_block(&provider, messages, 4, 2_048).await;
        assert_eq!(freed, 0, "summary bigger than original → skip");
    }
}
