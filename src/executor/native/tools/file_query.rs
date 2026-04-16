//! Survey-file tool: stream-read a file to answer a focused question.
//!
//! Spins up a mini agent loop with three tightly-scoped tools —
//! `read_chunk`, `note`, `finish` — and lets the agent traverse the
//! file chunk by chunk, carrying running notes out of context, until
//! it reaches an answer or exhausts its turn budget.
//!
//! ## Why this exists, and why not `recursive_summarize`
//!
//! `recursive_summarize` is map-reduce: each chunk is summarized in
//! isolation, summaries are concatenated, the result is re-summarized.
//! That's the right shape for "give me an overview of this document."
//! It's the wrong shape for "find the specific passage where X
//! happens" or "where does section 3 contradict section 8" — the
//! chunks don't see each other, so cumulative/cross-referential
//! questions can't be answered correctly.
//!
//! This tool is the other shape: one agent, cursor-based, carries
//! running state forward. The agent decides when to read next, when
//! to take notes, when it's seen enough. The out-of-context note
//! storage means notes survive naturally no matter how much the
//! in-context conversation compacts.
//!
//! ## Compaction strategy
//!
//! Each `read_chunk` tool call injects the chunk's text into the
//! message history as a tool_result block. Without bounds this blows
//! the context window. After every turn we replace all but the most
//! recent `read_chunk` tool_result with a stub ("[chunk N already
//! read; summary in notes]"). The agent's job is to extract what it
//! needs into notes *before* the next read_chunk. Notes live in the
//! tool-side `SurveyState`, which is not in message history — they
//! survive unchanged.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolOutput, ToolRegistry, truncate_tool_output};
use crate::executor::native::client::{
    ContentBlock, Message, MessagesRequest, Role, StopReason, ToolDefinition,
};
use crate::executor::native::provider::Provider;

/// Default max turns for a survey run. One turn = one LLM call.
/// Must be high enough to let the agent iterate through many chunks.
const DEFAULT_MAX_TURNS: usize = 40;

/// Hard cap on max_turns — prevents a stuck agent from burning API
/// calls indefinitely.
const MAX_ALLOWED_TURNS: usize = 100;

/// Fraction of the provider's context window to target per chunk.
/// Leaves headroom for notes + the model's response + tool definitions.
const CHUNK_WINDOW_FRACTION: f64 = 0.25;

/// Minimum chunk size in characters. Below this the overhead per-chunk
/// dominates and the tool is pointless.
const MIN_CHUNK_CHARS: usize = 4_000;

/// Maximum chunk size — keeps any one chunk comfortably under the
/// model's input limit even when the provider reports a large window.
const MAX_CHUNK_CHARS: usize = 80_000;

/// Maximum output chars for the final survey result.
const MAX_OUTPUT_CHARS: usize = 16_000;

const SURVEY_SYSTEM_PROMPT: &str = "\
You are surveying a file to answer a specific question. You have three tools:

  - read_chunk(n): read chunk number n of the file (0-indexed)
  - note(text): append a short note to your running notebook. Notes
    persist out-of-context — use them to capture anything important
    you've learned. They are your only durable memory across turns.
  - finish(answer): finalize your answer and end the survey.

Workflow:
  1. Start by reading chunk 0.
  2. After each read_chunk, take notes on anything relevant to the
     question BEFORE reading the next chunk. Old chunk contents get
     replaced with stubs in your context to keep memory bounded —
     if you didn't note it, it's gone.
  3. Continue through chunks in whatever order makes sense. You can
     revisit earlier chunks if needed.
  4. When you have enough to answer, call finish(answer) with a clear,
     direct answer to the question. Reference specific passages by
     their chunk number when relevant.

If you reach the last chunk without calling finish, do one more note()
to consolidate, then call finish with your best answer based on notes.
Never call finish with an empty answer — if you cannot answer, say so
explicitly and explain what you saw.";

pub fn register_survey_tool(registry: &mut ToolRegistry, workgraph_dir: PathBuf) {
    registry.register(Box::new(SurveyFileTool { workgraph_dir }));
}

struct SurveyFileTool {
    workgraph_dir: PathBuf,
}

#[async_trait]
impl Tool for SurveyFileTool {
    fn name(&self) -> &str {
        "survey_file"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "survey_file".to_string(),
            description: "Read a large file to answer a focused question. Spawns a sub-agent \
                          with read_chunk/note/finish tools that traverses the file chunk by \
                          chunk, carrying running notes out of context. Use for questions that \
                          need to scan a file too large to fit in context — 'find the passage \
                          where X happens', 'what's the contradiction between sections 3 and \
                          8', 'timeline of mentions of Y'. For a simple summary of a whole \
                          document, `summarize` is lighter; for cross-referential or \
                          cumulative questions, use this."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to survey"
                    },
                    "question": {
                        "type": "string",
                        "description": "The question to answer by reading the file"
                    },
                    "max_turns": {
                        "type": "integer",
                        "description": "Maximum conversation turns (default: 40, max: 100). \
                                        One turn per tool use. Larger files need more turns."
                    }
                },
                "required": ["path", "question"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return ToolOutput::error("Missing or empty parameter: path".to_string()),
        };
        let question = match input.get("question").and_then(|v| v.as_str()) {
            Some(q) if !q.trim().is_empty() => q.trim().to_string(),
            _ => return ToolOutput::error("Missing or empty parameter: question".to_string()),
        };
        let max_turns = input
            .get("max_turns")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, MAX_ALLOWED_TURNS))
            .unwrap_or(DEFAULT_MAX_TURNS);

        // Resolve model + provider the same way research/deep_research do:
        // WG_MODEL env var > config.resolve_model_for_role(TaskAgent) > provider default.
        let config = crate::config::Config::load_or_default(&self.workgraph_dir);
        let model = std::env::var("WG_MODEL")
            .ok()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| {
                config
                    .resolve_model_for_role(crate::config::DispatchRole::TaskAgent)
                    .model
            });
        let provider =
            match crate::executor::native::provider::create_provider(&self.workgraph_dir, &model) {
                Ok(p) => p,
                Err(e) => {
                    return ToolOutput::error(format!(
                        "survey_file: failed to create provider (model {}): {}",
                        model, e
                    ));
                }
            };

        match run_survey(provider.as_ref(), &path, &question, max_turns).await {
            Ok(result) => ToolOutput::success(truncate_tool_output(&result, MAX_OUTPUT_CHARS)),
            Err(e) => ToolOutput::error(format!("survey_file failed: {}", e)),
        }
    }
}

/// Shared state across a survey run. The `read_chunk` / `note` /
/// `finish` tools all hold an `Arc<Mutex<SurveyState>>`.
#[derive(Debug)]
struct SurveyState {
    chunks: Vec<String>,
    notes: Vec<String>,
    chunks_read: HashSet<usize>,
    final_answer: Option<String>,
}

type SurveyStateRef = Arc<Mutex<SurveyState>>;

/// Chunk a file's contents into `chunk_chars`-sized pieces, breaking
/// on paragraph boundaries (double newline) when possible to keep
/// semantic units intact. Falls back to line, then to hard split.
pub(crate) fn chunk_text(text: &str, chunk_chars: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    if text.len() <= chunk_chars {
        return vec![text.to_string()];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + chunk_chars).min(text.len());
        if end < text.len() {
            // Prefer paragraph boundary in the last 30% of the chunk.
            let boundary_search_start = start + (chunk_chars * 7 / 10);
            if let Some(pos) = text[boundary_search_start..end].rfind("\n\n") {
                end = boundary_search_start + pos + 2;
            } else if let Some(pos) = text[boundary_search_start..end].rfind('\n') {
                end = boundary_search_start + pos + 1;
            } else {
                // Hard-split respecting char boundaries
                while end > start && !text.is_char_boundary(end) {
                    end -= 1;
                }
            }
        }
        if end <= start {
            break;
        }
        chunks.push(text[start..end].to_string());
        start = end;
    }
    chunks
}

/// Compute target chunk size from provider context window.
fn compute_chunk_size(context_window: usize) -> usize {
    // context_window is in TOKENS; assume ~3 chars per token average (Anglophone bias)
    // for file content with code+prose mixed. Err conservative.
    let chars = (context_window as f64 * CHUNK_WINDOW_FRACTION * 3.0) as usize;
    chars.clamp(MIN_CHUNK_CHARS, MAX_CHUNK_CHARS)
}

/// Compact the message history in place: replace all but the most
/// recent `read_chunk` tool_result with a stub that preserves the
/// chunk index reference but drops the chunk text. Notes remain
/// untouched (they live in SurveyState, not in messages).
fn compact_read_chunk_results(messages: &mut Vec<Message>) {
    // Find indices of every tool_result block that came from read_chunk.
    // The full text of a read_chunk result starts with "chunk N/M:" — we
    // pattern-match on that prefix to identify which results came from
    // read_chunk vs. the other tools.
    let mut read_chunk_positions = Vec::new();
    for (msg_idx, msg) in messages.iter().enumerate() {
        if msg.role != Role::User {
            continue;
        }
        for (block_idx, block) in msg.content.iter().enumerate() {
            if let ContentBlock::ToolResult { content, .. } = block
                && content.starts_with("chunk ")
                && content.contains("/")
                && content.contains(":\n")
            {
                read_chunk_positions.push((msg_idx, block_idx));
            }
        }
    }

    // Keep only the most recent one; stub all earlier ones.
    if read_chunk_positions.len() <= 1 {
        return;
    }
    let keep = *read_chunk_positions.last().unwrap();
    for (msg_idx, block_idx) in &read_chunk_positions {
        if (*msg_idx, *block_idx) == keep {
            continue;
        }
        if let Some(msg) = messages.get_mut(*msg_idx)
            && let Some(block) = msg.content.get_mut(*block_idx)
            && let ContentBlock::ToolResult {
                content, is_error, ..
            } = block
        {
            // Pull chunk index from the header for the stub message.
            let header = content.lines().next().unwrap_or("").to_string();
            *content = format!(
                "[{} — full text dropped to save context; \
                 if you didn't capture it in notes, re-read via read_chunk]",
                header.trim_end_matches(':').trim()
            );
            *is_error = false;
        }
    }
}

/// Main survey loop. Reads the file, chunks it, runs the mini agent
/// loop until finish() is called or max_turns is exhausted.
async fn run_survey(
    provider: &dyn Provider,
    path: &str,
    question: &str,
    max_turns: usize,
) -> Result<String, String> {
    // Load + chunk the file.
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read file '{}': {}", path, e))?;
    let chunk_chars = compute_chunk_size(provider.context_window());
    let chunks = chunk_text(&text, chunk_chars);
    if chunks.is_empty() {
        return Ok(format!(
            "File '{}' is empty. Cannot answer: {}",
            path, question
        ));
    }
    let n_chunks = chunks.len();

    eprintln!(
        "[survey] {} → {} chunk(s) of ~{} chars, question: {:?}",
        path,
        n_chunks,
        chunk_chars,
        truncate(question, 80)
    );

    let state = Arc::new(Mutex::new(SurveyState {
        chunks,
        notes: Vec::new(),
        chunks_read: HashSet::new(),
        final_answer: None,
    }));

    // Build registry with only the three survey tools.
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ReadChunkTool {
        state: state.clone(),
    }));
    registry.register(Box::new(NoteTool {
        state: state.clone(),
    }));
    registry.register(Box::new(FinishTool {
        state: state.clone(),
    }));

    let tool_defs = registry.definitions();
    let initial_user = format!(
        "Question: {}\n\nThe file is split into {} chunks (indices 0..{}). \
         Start with read_chunk(0) and work forward.",
        question,
        n_chunks,
        n_chunks - 1
    );
    let mut messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: initial_user }],
    }];

    for turn in 0..max_turns {
        // Inject current notes count into the conversation so the agent
        // has situational awareness. Small overhead.
        let (note_count, read_count) = {
            let s = state.lock().unwrap();
            (s.notes.len(), s.chunks_read.len())
        };
        eprintln!(
            "[survey] turn {}/{} (chunks_read={}/{}, notes={})",
            turn + 1,
            max_turns,
            read_count,
            n_chunks,
            note_count
        );

        let request = MessagesRequest {
            model: provider.model().to_string(),
            max_tokens: provider.max_tokens(),
            system: Some(SURVEY_SYSTEM_PROMPT.to_string()),
            messages: messages.clone(),
            tools: tool_defs.clone(),
            stream: false,
        };
        let response = provider
            .send(&request)
            .await
            .map_err(|e| format!("API error on turn {}: {}", turn + 1, e))?;

        messages.push(Message {
            role: Role::Assistant,
            content: response.content.clone(),
        });

        match response.stop_reason {
            Some(StopReason::EndTurn) | Some(StopReason::StopSequence) | None => {
                // Agent ended its turn without a tool call. If it called
                // finish() earlier, we already have final_answer. If not,
                // fall back to synthesizing from notes.
                let s = state.lock().unwrap();
                if let Some(ref answer) = s.final_answer {
                    return Ok(answer.clone());
                }
                // Agent is just talking — nudge it to use tools.
                drop(s);
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Use read_chunk, note, or finish. Do not reply with plain text \
                               — you have no durable memory outside notes."
                            .to_string(),
                    }],
                });
                continue;
            }
            Some(StopReason::MaxTokens) => {
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Your response was truncated. Use finish() with a concise answer \
                               based on what you've noted so far."
                            .to_string(),
                    }],
                });
                continue;
            }
            Some(StopReason::ToolUse) => {
                let tool_uses: Vec<_> = response
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => {
                            Some((id.clone(), name.clone(), input.clone()))
                        }
                        _ => None,
                    })
                    .collect();

                let mut results = Vec::new();
                for (id, name, input) in &tool_uses {
                    let output = registry.execute(name, input).await;
                    results.push(ContentBlock::ToolResult {
                        tool_use_id: id.clone(),
                        content: output.content.clone(),
                        is_error: output.is_error,
                    });
                }
                messages.push(Message {
                    role: Role::User,
                    content: results,
                });

                // Check for finish() signal.
                let s = state.lock().unwrap();
                if let Some(ref answer) = s.final_answer {
                    return Ok(answer.clone());
                }
                drop(s);

                // Bound context: replace older read_chunk outputs with stubs.
                compact_read_chunk_results(&mut messages);
            }
        }
    }

    // Max turns reached without finish. Synthesize from notes.
    let s = state.lock().unwrap();
    if let Some(ref answer) = s.final_answer {
        return Ok(answer.clone());
    }
    if s.notes.is_empty() {
        Ok(format!(
            "[survey: reached max turns ({}) without answer or notes. \
             Chunks read: {}/{}]",
            max_turns,
            s.chunks_read.len(),
            n_chunks
        ))
    } else {
        let notes = s.notes.join("\n\n");
        Ok(format!(
            "[survey: reached max turns ({}) without explicit finish — returning running notes]\n\n\
             Question: {}\n\n\
             Notes:\n{}",
            max_turns, question, notes
        ))
    }
}

// ─── Tool: read_chunk ───────────────────────────────────────────────────

struct ReadChunkTool {
    state: SurveyStateRef,
}

#[async_trait]
impl Tool for ReadChunkTool {
    fn name(&self) -> &str {
        "read_chunk"
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_chunk".to_string(),
            description: "Read chunk N of the file (0-indexed). Extract what you need into \
                          notes BEFORE the next read_chunk — old chunks are compacted to \
                          stubs to save context."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "n": {
                        "type": "integer",
                        "description": "0-indexed chunk number"
                    }
                },
                "required": ["n"]
            }),
        }
    }
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let n = match input.get("n").and_then(|v| v.as_i64()) {
            Some(n) if n >= 0 => n as usize,
            _ => return ToolOutput::error("Parameter 'n' must be a non-negative integer".into()),
        };
        let mut s = self.state.lock().unwrap();
        let total = s.chunks.len();
        if n >= total {
            return ToolOutput::error(format!(
                "Chunk index {} out of range (file has {} chunks, 0..{})",
                n,
                total,
                total - 1
            ));
        }
        let text = s.chunks[n].clone();
        s.chunks_read.insert(n);
        // Header format is load-bearing for compact_read_chunk_results' detection.
        ToolOutput::success(format!("chunk {}/{}:\n{}", n, total, text))
    }
}

// ─── Tool: note ─────────────────────────────────────────────────────────

struct NoteTool {
    state: SurveyStateRef,
}

#[async_trait]
impl Tool for NoteTool {
    fn name(&self) -> &str {
        "note"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "note".to_string(),
            description: "Append a note to your out-of-context notebook. Notes persist across \
                          turns and survive context compaction. Keep notes concise — this is \
                          your durable memory."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "Note content"
                    }
                },
                "required": ["text"]
            }),
        }
    }
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let text = match input.get("text").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => return ToolOutput::error("Parameter 'text' must be non-empty".into()),
        };
        let mut s = self.state.lock().unwrap();
        s.notes.push(text);
        let total_notes = s.notes.len();
        ToolOutput::success(format!("Note #{} saved.", total_notes))
    }
}

// ─── Tool: finish ───────────────────────────────────────────────────────

struct FinishTool {
    state: SurveyStateRef,
}

#[async_trait]
impl Tool for FinishTool {
    fn name(&self) -> &str {
        "finish"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "finish".to_string(),
            description: "Finalize your answer and end the survey. Pass the complete answer \
                          as `answer`. This terminates the survey loop — after finish is \
                          called, no further tool calls will be processed."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "answer": {
                        "type": "string",
                        "description": "The final, complete answer to the question"
                    }
                },
                "required": ["answer"]
            }),
        }
    }
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let answer = match input.get("answer").and_then(|v| v.as_str()) {
            Some(a) if !a.trim().is_empty() => a.trim().to_string(),
            _ => return ToolOutput::error("Parameter 'answer' must be non-empty".into()),
        };
        let mut s = self.state.lock().unwrap();
        s.final_answer = Some(answer);
        ToolOutput::success("Survey finished. Loop will exit.".to_string())
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut i = max;
        while i > 0 && !s.is_char_boundary(i) {
            i -= 1;
        }
        &s[..i]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_text_empty() {
        assert!(chunk_text("", 100).is_empty());
    }

    #[test]
    fn chunk_text_smaller_than_window_single_chunk() {
        let text = "hello world";
        let chunks = chunk_text(text, 100);
        assert_eq!(chunks, vec!["hello world".to_string()]);
    }

    #[test]
    fn chunk_text_splits_on_paragraph_boundary() {
        let text = "first para\n\nsecond para\n\nthird para\n\nfourth para";
        let chunks = chunk_text(text, 15);
        assert!(chunks.len() >= 2, "should split: got {:?}", chunks);
        // Each chunk should end at a reasonable boundary (no mid-word splits).
        for c in &chunks {
            assert!(!c.is_empty());
        }
        // Reassembling should give back the original.
        assert_eq!(chunks.join(""), text);
    }

    #[test]
    fn chunk_text_splits_on_line_when_no_paragraphs() {
        let text = "line 1\nline 2\nline 3\nline 4\nline 5";
        let chunks = chunk_text(text, 15);
        assert!(chunks.len() >= 2, "should split");
        assert_eq!(chunks.join(""), text);
    }

    #[test]
    fn chunk_text_preserves_all_content() {
        let text = "a".repeat(1000);
        let chunks = chunk_text(&text, 100);
        let reassembled: String = chunks.concat();
        assert_eq!(reassembled.len(), text.len());
        assert_eq!(reassembled, text);
    }

    #[test]
    fn compute_chunk_size_respects_clamps() {
        // Tiny window — should hit MIN_CHUNK_CHARS
        assert_eq!(compute_chunk_size(100), MIN_CHUNK_CHARS);
        // Huge window — should hit MAX_CHUNK_CHARS
        assert_eq!(compute_chunk_size(1_000_000), MAX_CHUNK_CHARS);
        // Reasonable window — should compute intermediate
        let mid = compute_chunk_size(32_768);
        assert!(mid >= MIN_CHUNK_CHARS);
        assert!(mid <= MAX_CHUNK_CHARS);
    }

    #[test]
    fn compact_stubs_all_but_last_read_chunk_result() {
        let mut messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "id1".into(),
                    content: "chunk 0/3:\nOriginal chunk 0 content here".into(),
                    is_error: false,
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "taking notes".into(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "id2".into(),
                    content: "chunk 1/3:\nOriginal chunk 1 content here".into(),
                    is_error: false,
                }],
            },
        ];
        compact_read_chunk_results(&mut messages);

        let first_content = match &messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => content,
            _ => panic!("expected tool result"),
        };
        assert!(
            first_content.contains("full text dropped"),
            "first chunk should be stubbed, got: {}",
            first_content
        );
        assert!(first_content.contains("chunk 0/3"));

        let last_content = match &messages[2].content[0] {
            ContentBlock::ToolResult { content, .. } => content,
            _ => panic!("expected tool result"),
        };
        assert!(
            last_content.contains("Original chunk 1 content"),
            "most recent chunk should NOT be stubbed, got: {}",
            last_content
        );
    }

    #[test]
    fn compact_leaves_non_readchunk_results_alone() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "id1".into(),
                content: "Note #1 saved.".into(),
                is_error: false,
            }],
        }];
        let before = messages.clone();
        compact_read_chunk_results(&mut messages);
        // Pattern doesn't match "chunk N/M:" so it's left alone.
        match &messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, "Note #1 saved.");
            }
            _ => panic!("expected tool result"),
        }
        let _ = before; // anchor
    }

    #[tokio::test]
    async fn read_chunk_returns_content_and_tracks_reads() {
        let state = Arc::new(Mutex::new(SurveyState {
            chunks: vec!["first".into(), "second".into(), "third".into()],
            notes: Vec::new(),
            chunks_read: HashSet::new(),
            final_answer: None,
        }));
        let tool = ReadChunkTool {
            state: state.clone(),
        };
        let out = tool.execute(&json!({"n": 1})).await;
        assert!(!out.is_error);
        assert!(out.content.contains("chunk 1/3:"));
        assert!(out.content.contains("second"));
        let s = state.lock().unwrap();
        assert!(s.chunks_read.contains(&1));
    }

    #[tokio::test]
    async fn read_chunk_errors_on_out_of_range() {
        let state = Arc::new(Mutex::new(SurveyState {
            chunks: vec!["only".into()],
            notes: Vec::new(),
            chunks_read: HashSet::new(),
            final_answer: None,
        }));
        let tool = ReadChunkTool { state };
        let out = tool.execute(&json!({"n": 5})).await;
        assert!(out.is_error);
        assert!(out.content.contains("out of range"));
    }

    #[tokio::test]
    async fn note_appends_and_errors_on_empty() {
        let state = Arc::new(Mutex::new(SurveyState {
            chunks: vec![],
            notes: Vec::new(),
            chunks_read: HashSet::new(),
            final_answer: None,
        }));
        let tool = NoteTool {
            state: state.clone(),
        };
        let out = tool.execute(&json!({"text": "important!"})).await;
        assert!(!out.is_error);
        assert_eq!(state.lock().unwrap().notes, vec!["important!".to_string()]);
        // empty
        let out2 = tool.execute(&json!({"text": "   "})).await;
        assert!(out2.is_error);
    }

    #[tokio::test]
    async fn finish_stores_answer_and_short_circuits_loop() {
        let state = Arc::new(Mutex::new(SurveyState {
            chunks: vec![],
            notes: Vec::new(),
            chunks_read: HashSet::new(),
            final_answer: None,
        }));
        let tool = FinishTool {
            state: state.clone(),
        };
        let out = tool.execute(&json!({"answer": "the answer is 42"})).await;
        assert!(!out.is_error);
        assert_eq!(
            state.lock().unwrap().final_answer,
            Some("the answer is 42".to_string())
        );
    }
}
