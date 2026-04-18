//! Reader tool: sub-executor with a working directory, sequential
//! chunk pull, and writable scratch space.
//!
//! Shape (from the 2026-04-16 design exchange):
//!
//!   reader(path, task) → working_dir_path
//!
//! Spawns a mini agent loop with seven tightly-scoped tools:
//!
//!   - `next_chunk(size)` — returns the next N chars of the input
//!     file starting at the cursor, advances the cursor. Sequential;
//!     the agent doesn't track indices. EOF returns a clear marker.
//!   - `write_note(name, content)` — writes a file in the working dir.
//!     Overwrite semantics; caller can pick the filename.
//!   - `append_note(name, content)` — appends to a file in the
//!     working dir (creates if missing).
//!   - `list_notes()` — what's in the working dir so far.
//!   - `read_note(name)` — reads back a note the agent (or an
//!     earlier turn) wrote.
//!   - `bash(command)` — shell command with cwd set to the working
//!     dir. `grep`/`sed`/`awk`/etc. on the accumulated notes.
//!   - `finish(result)` — terminates the loop with a final answer.
//!
//! The working directory lives at
//! `<workgraph_dir>/readers/<timestamp>-<slug>/` and **persists**
//! after the reader exits. The outer session can `cat` / `ls` it to
//! inspect everything the reader produced. Readers are sacred — not
//! auto-removed — the same philosophy as worktrees.
//!
//! Why this exists and why not just `read_file(path, query)`:
//!
//!   - `read_file(path, query)` is single-shot. When the file doesn't
//!     fit in one LLM call, it errors out and points here.
//!   - `reader` handles arbitrarily-large files by letting the agent
//!     pull chunks on its own schedule, write running notes to disk,
//!     and compose them. Notes survive across compaction because
//!     they live on disk, not in message history.
//!   - Output shape is genuinely different: `read_file(query)`
//!     returns a text answer; `reader` returns a **workspace** the
//!     outer agent can explore. For complex tasks (summarize this
//!     book AND produce per-chapter notes AND cross-reference
//!     against another file), a workspace is the right primitive.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolOutput, ToolRegistry};
use crate::executor::native::client::{
    ContentBlock, Message, MessagesRequest, Role, StopReason, ToolDefinition,
};

/// Default chunk size in characters when the agent doesn't specify.
/// ~8K chars ≈ 2K tokens — a reasonable "page" of reading.
const DEFAULT_CHUNK_CHARS: usize = 8_000;

/// Hard cap on a single chunk — keeps any one pull bounded even if
/// the agent asks for something absurd.
const MAX_CHUNK_CHARS: usize = 40_000;

/// Minimum chunk — below this the per-turn overhead dominates.
const MIN_CHUNK_CHARS: usize = 512;

/// Floor on auto-sized max_turns: even tiny files need room for
/// the initial read, a note, and a finalize.
const MIN_AUTO_TURNS: usize = 10;

/// Hard cap on max_turns — prevents runaway cost.
const MAX_ALLOWED_TURNS: usize = 200;

/// Compute a reasonable `max_turns` for a reader run given the
/// total size of the input file.
///
/// Model: the sub-agent reads the file in `DEFAULT_CHUNK_CHARS`-sized
/// chunks, keeping notes along the way. A rough cost model is:
///
///   turns ≈ ceil(content_len / DEFAULT_CHUNK_CHARS) * (1 + overhead)
///
/// where `overhead` covers the per-chunk note-taking turn(s) and
/// the final synthesis. We use `1.3` (30%) which matches what
/// in-practice reader runs burn.
///
/// Clamped to `[MIN_AUTO_TURNS, MAX_ALLOWED_TURNS]` so small files
/// get enough turns to complete and huge files don't trigger
/// runaway cost.
pub(crate) fn auto_size_max_turns(content_len: usize) -> usize {
    if content_len == 0 {
        return MIN_AUTO_TURNS;
    }
    let chunks = content_len.div_ceil(DEFAULT_CHUNK_CHARS);
    // 1 turn per chunk read + ~30% for synthesis/notes.
    let est = ((chunks as f64) * 1.3).ceil() as usize;
    est.clamp(MIN_AUTO_TURNS, MAX_ALLOWED_TURNS)
}

/// Cap on the size of any note file. Prevents a runaway agent from
/// filling the disk. A 1 MB note is bigger than most books' main text.
const MAX_NOTE_CHARS: usize = 1_024 * 1_024;

/// Cap on returned `read_note` content to keep tool_result blocks
/// bounded in the agent's context.
const MAX_READ_NOTE_CHARS: usize = 40_000;

/// Timeout for a single bash command invocation inside the reader.
const BASH_TIMEOUT_SECS: u64 = 30;

const READER_SYSTEM_PROMPT: &str = "\
You are reading a large file to accomplish a task. Your conversation \
context is SMALL and will be compacted aggressively. Your working \
directory is your actual memory — files you write there persist \
forever. Don't forget this.

## The core rule

**Between every two next_chunk() calls, you MUST call append_note() or \
write_note() to capture what you saw.** Old next_chunk outputs get \
replaced with a stub in your context on the next turn — if you didn't \
save it to disk, the content is gone forever. The sequential cursor \
means you can't go back. Pattern:

  next_chunk → append_note(findings) → next_chunk → append_note → ...

Calling next_chunk twice in a row without a note in between IS A BUG, \
even if the agent framework doesn't reject it — you'll just lose data.

## Tools

  - next_chunk(size): read the next `size` chars at the cursor, \
    advancing the cursor. Default ~8000. Returns 'EOF' at end of file.
  - append_note(name, content): append to a file in your working \
    directory. Creates if missing. **Use this after every chunk.** \
    Pattern: `append_note('findings.md', '## Line X-Y\\n- observed: ...')`.
  - write_note(name, content): create/overwrite a file. Use for \
    structured artifacts — the final outline, a cross-reference table, \
    a synthesis.
  - list_notes(): list files in the working directory.
  - read_note(name): read back an earlier note.
  - bash(command): shell with cwd set to working directory. Great for \
    `grep` / `wc` / `cat` combining notes.
  - finish(result): terminate. Result string is shown to the caller \
    along with your working directory path — so put durable output \
    IN NOTES, not just in the result text.

## Workflow

  1. next_chunk() — first page of the file
  2. append_note('findings.md', ...) — capture what you observed
  3. Repeat (1+2) until you have enough or hit EOF
  4. Optionally: use bash/read_note to review/cross-reference notes
  5. finish(result) — concise answer; the full details live in notes

## Failure modes to avoid

  - Calling next_chunk repeatedly without notes → data loss + context \
    overflow → you will hit an API error and fail the task.
  - Answering from memory of a chunk you didn't note → fabrication. \
    If you didn't write it down, admit you don't have it.
  - Calling finish before reading enough — notes < answer.";

pub fn register_reader_tool(registry: &mut ToolRegistry, workgraph_dir: PathBuf) {
    registry.register(Box::new(ReaderTool { workgraph_dir }));
}

struct ReaderTool {
    workgraph_dir: PathBuf,
}

#[async_trait]
impl Tool for ReaderTool {
    fn name(&self) -> &str {
        "reader"
    }

    fn is_read_only(&self) -> bool {
        // reader writes to its own working directory (not the user's
        // source tree), so from the outer perspective this is read-only
        // on the user's code. The notes directory is the artifact.
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "reader".to_string(),
            description: "Run a sub-agent over a large file to accomplish a task. The \
                          sub-agent reads the file sequentially in chunks it pulls on \
                          demand, writes running notes to a dedicated working directory, \
                          and terminates with a final result. The working directory \
                          persists after completion — you can `ls` and `cat` its contents \
                          to see everything the sub-agent produced.\n\
                          \n\
                          Use this for files too large for `read_file(path, query)`'s \
                          single-shot mode, or for tasks that benefit from a workspace \
                          (summarize a book AND produce per-chapter notes, cross-reference \
                          a long document against a question, extract every mention of X \
                          from a 10MB log). Returns the path to the working directory plus \
                          the sub-agent's finish() result."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path to the input file to read"
                    },
                    "task": {
                        "type": "string",
                        "description": "What the sub-agent should accomplish. Be specific \
                                        about the output you want — 'find every mention \
                                        of X and list line numbers', 'summarize chapter by \
                                        chapter in chapters.md', etc."
                    },
                    "max_turns": {
                        "type": "integer",
                        "description": "Max conversation turns (default 50, cap 200). \
                                        One turn = one LLM call."
                    }
                },
                "required": ["path", "task"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return ToolOutput::error("Missing or empty parameter: path".to_string()),
        };
        let task = match input.get("task").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => return ToolOutput::error("Missing or empty parameter: task".to_string()),
        };
        // Resolve max_turns. Explicit caller value wins. Otherwise
        // size it to the content: enough turns to read the file in
        // `DEFAULT_CHUNK_CHARS`-sized bites plus a ~30% synthesis
        // overhead for note-taking + finalization, with a floor of
        // 10 (so small files aren't starved) and a hard ceiling of
        // MAX_ALLOWED_TURNS (200). The old fixed 50 was both too
        // many for 5kb files (wastes LLM budget) and too few for
        // 500kb files (fails before synthesis).
        let max_turns = match input.get("max_turns").and_then(|v| v.as_u64()) {
            Some(n) => (n as usize).clamp(1, MAX_ALLOWED_TURNS),
            None => {
                let content_len = std::fs::metadata(&path)
                    .map(|m| m.len() as usize)
                    .unwrap_or(0);
                auto_size_max_turns(content_len)
            }
        };

        match run_reader(&self.workgraph_dir, &path, &task, max_turns).await {
            Ok(result) => ToolOutput::success(result),
            Err(e) => ToolOutput::error(format!("reader failed: {}", e)),
        }
    }
}

/// Shared state across a reader run. Each sub-tool holds an Arc<Mutex<_>>.
struct ReaderState {
    input_text: String,
    cursor: usize,
    working_dir: PathBuf,
    final_result: Option<String>,
}

type ReaderStateRef = Arc<Mutex<ReaderState>>;

/// Main reader loop. Creates a working dir, spawns the mini agent loop,
/// returns path+result when finish() is called or max_turns is reached.
async fn run_reader(
    workgraph_dir: &Path,
    path: &str,
    task: &str,
    max_turns: usize,
) -> Result<String, String> {
    let input_text =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read '{}': {}", path, e))?;
    let total_chars = input_text.len();

    // Create working directory. Lives at <workgraph_dir>/readers/<stamp>-<slug>/
    // and persists after the reader exits.
    let working_dir = make_working_dir(workgraph_dir, path)?;
    eprintln!(
        "[reader] start: path={}, task={:?}, working_dir={}, total_chars={}",
        path,
        truncate(task, 80),
        working_dir.display(),
        total_chars
    );

    // Resolve provider via the usual chain.
    let config = crate::config::Config::load_or_default(workgraph_dir);
    let model = std::env::var("WG_MODEL")
        .ok()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| {
            config
                .resolve_model_for_role(crate::config::DispatchRole::TaskAgent)
                .model
        });
    let provider = crate::executor::native::provider::create_provider(workgraph_dir, &model)
        .map_err(|e| format!("create provider (model {}): {}", model, e))?;

    let state = Arc::new(Mutex::new(ReaderState {
        input_text,
        cursor: 0,
        working_dir: working_dir.clone(),
        final_result: None,
    }));

    // Build the reader's tool registry (7 tools, tightly scoped).
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(NextChunkTool {
        state: state.clone(),
    }));
    registry.register(Box::new(WriteNoteTool {
        state: state.clone(),
    }));
    registry.register(Box::new(AppendNoteTool {
        state: state.clone(),
    }));
    registry.register(Box::new(ListNotesTool {
        state: state.clone(),
    }));
    registry.register(Box::new(ReadNoteTool {
        state: state.clone(),
    }));
    registry.register(Box::new(BashTool {
        state: state.clone(),
    }));
    registry.register(Box::new(FinishTool {
        state: state.clone(),
    }));

    let tool_defs = registry.definitions();
    let initial_msg = format!(
        "Task: {}\n\nInput file: {} ({} chars total)\n\nStart by calling next_chunk() to read \
         the first page, then take notes and proceed. When done, call finish(result).",
        task, path, total_chars
    );
    let mut messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: initial_msg }],
    }];

    for turn in 0..max_turns {
        // Snapshot state BEFORE the turn so we can diff and report
        // exactly what changed. Reporting "cursor=3703/3703" six turns
        // in a row (as happened in the weather smoke test) tells the
        // operator nothing about what the agent actually did — these
        // action-based lines show the trajectory.
        let (cursor_before, notes_before) = {
            let s = state.lock().unwrap();
            (s.cursor, count_notes(&s.working_dir))
        };

        let request = MessagesRequest {
            model: provider.model().to_string(),
            max_tokens: provider.max_tokens(),
            system: Some(READER_SYSTEM_PROMPT.to_string()),
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

        // Extract what tools the model asked for on this turn (if any).
        let tool_names: Vec<String> = response
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();

        match response.stop_reason {
            Some(StopReason::EndTurn) | Some(StopReason::StopSequence) | None => {
                let s = state.lock().unwrap();
                if let Some(ref result) = s.final_result {
                    eprintln!(
                        "[reader] turn {}/{}: finish() (cursor {}/{}, notes {})",
                        turn + 1,
                        max_turns,
                        s.cursor,
                        total_chars,
                        count_notes(&s.working_dir),
                    );
                    return Ok(format_exit(&s.working_dir, result, turn + 1, false));
                }
                drop(s);
                eprintln!(
                    "[reader] turn {}/{}: (no tool call — plain text) cursor {}/{}, notes {}",
                    turn + 1,
                    max_turns,
                    cursor_before,
                    total_chars,
                    notes_before,
                );
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Use a tool (next_chunk, write_note, append_note, list_notes, \
                               read_note, bash, or finish). Plain text replies have no \
                               durable memory — the working dir does."
                            .to_string(),
                    }],
                });
                continue;
            }
            Some(StopReason::MaxTokens) => {
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Response was truncated. Call finish() with your best \
                               answer based on notes so far."
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
                let finished = s.final_result.is_some();
                let cursor_after = s.cursor;
                let notes_after = count_notes(&s.working_dir);
                let final_result_opt = s.final_result.clone();
                drop(s);

                // Action-based telemetry: name the tool(s) called and
                // show what actually changed this turn. "cursor 0→3703
                // (EOF), notes 0→1" tells you what the agent did.
                // "cursor=3703/3703 notes=1" printed six turns in a
                // row (the weather smoke-test failure mode) tells you
                // nothing about the trajectory.
                let action = tool_names.join(",");
                let cursor_delta = if cursor_after != cursor_before {
                    let pct = if total_chars == 0 {
                        100
                    } else {
                        (cursor_after * 100 / total_chars).min(100)
                    };
                    let eof_marker = if cursor_after >= total_chars {
                        " EOF"
                    } else {
                        ""
                    };
                    format!(
                        " cursor {}→{} ({}%{})",
                        cursor_before, cursor_after, pct, eof_marker
                    )
                } else {
                    String::new()
                };
                let notes_delta = if notes_after != notes_before {
                    format!(" notes {}→{}", notes_before, notes_after)
                } else {
                    String::new()
                };
                eprintln!(
                    "[reader] turn {}/{}: {}{}{}",
                    turn + 1,
                    max_turns,
                    action,
                    cursor_delta,
                    notes_delta,
                );

                if finished {
                    return Ok(format_exit(
                        &state.lock().unwrap().working_dir,
                        &final_result_opt.unwrap_or_default(),
                        turn + 1,
                        false,
                    ));
                }

                // Bound context: replace all but the most recent
                // next_chunk tool_result with a stub. Without this, a
                // greedy agent that calls next_chunk repeatedly without
                // noting anything will blow past the context window
                // within ~15 turns (observed: qwen3-coder-30b in smoke
                // test hit API 400 "token count exceeds model" at turn
                // 15 with 8K chunks, 32K context). Notes live in the
                // working dir on disk so they survive compaction; the
                // chunk text itself was supposed to be extracted into
                // notes before the next chunk — this enforces the
                // "notes are your durable memory" invariant mechanically
                // rather than relying on the agent to follow guidance.
                compact_next_chunk_results(&mut messages);
            }
        }
    }

    // Exhausted turns without finish. Return what we have.
    let s = state.lock().unwrap();
    let fallback = s
        .final_result
        .clone()
        .unwrap_or_else(|| "[reader: max turns reached without finish()]".to_string());
    Ok(format_exit(&s.working_dir, &fallback, max_turns, true))
}

/// Create the working dir at `<workgraph_dir>/readers/<stamp>-<slug>/`.
fn make_working_dir(workgraph_dir: &Path, input_path: &str) -> Result<PathBuf, String> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let slug = slug_from_path(input_path);
    let dir = workgraph_dir
        .join("readers")
        .join(format!("{}-{}", stamp, slug));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create working dir {:?}: {}", dir, e))?;
    Ok(dir)
}

/// Slug from the input path: basename, alphanumeric + dashes only,
/// capped at 40 chars.
fn slug_from_path(path: &str) -> String {
    let base = std::path::Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "input".to_string());
    let mut out: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Collapse runs of '-'
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out = out.trim_matches('-').to_string();
    if out.is_empty() {
        out = "input".to_string();
    }
    if out.len() > 40 {
        out.truncate(40);
    }
    out
}

/// Format the reader's exit message: working dir + result + stats.
fn format_exit(working_dir: &Path, result: &str, turns: usize, hit_max: bool) -> String {
    let status = if hit_max { " (HIT MAX TURNS)" } else { "" };
    format!(
        "Reader result:\n{}\n\n--- Reader metadata ---\nWorking directory: {}\nTurns used: {}{}\n\
         Inspect the working directory to see notes, artifacts, and any files the sub-agent \
         wrote. Use `bash ls` and `cat` / `read_file` on specific paths.",
        result,
        working_dir.display(),
        turns,
        status,
    )
}

/// Compact old `next_chunk` tool_results: keep the most recent one at
/// full fidelity, replace all earlier ones with a short stub that
/// preserves the position reference but drops the chunk text.
///
/// Detects next_chunk output by the distinctive prefix `"[chunk "` +
/// `" of "` + `" chars, "` in the content — this shape comes from
/// `NextChunkTool::execute`. Other tool_results (write_note, note-list,
/// read_note, bash, finish) are left untouched.
///
/// Notes stay in `SurveyState`/`ReaderState` on disk, not in message
/// history, so they survive compaction unchanged. The agent is
/// therefore forced to extract what it needs into notes BEFORE
/// calling next_chunk again, or the information is lost.
fn compact_next_chunk_results(messages: &mut [Message]) {
    let mut positions = Vec::new();
    for (msg_idx, msg) in messages.iter().enumerate() {
        if msg.role != Role::User {
            continue;
        }
        for (block_idx, block) in msg.content.iter().enumerate() {
            if let ContentBlock::ToolResult { content, .. } = block
                && is_next_chunk_output(content)
            {
                positions.push((msg_idx, block_idx));
            }
        }
    }
    if positions.len() <= 1 {
        return;
    }
    let keep = *positions.last().unwrap();
    for (msg_idx, block_idx) in &positions {
        if (*msg_idx, *block_idx) == keep {
            continue;
        }
        if let Some(msg) = messages.get_mut(*msg_idx)
            && let Some(block) = msg.content.get_mut(*block_idx)
            && let ContentBlock::ToolResult {
                content, is_error, ..
            } = block
        {
            // Pull the "[chunk START..END of TOTAL chars, N% through file]"
            // header and keep only that reference, dropping the text body.
            let header = content.lines().next().unwrap_or("").to_string();
            *content = format!(
                "{} — full text dropped to save context. If you didn't capture it \
                 in a note, call next_chunk with an earlier position is not supported \
                 (this tool is sequential); you'd need to restart.",
                header
            );
            *is_error = false;
        }
    }
}

/// True if `content` is the output of a successful `next_chunk` call —
/// i.e., starts with the distinctive header the tool produces.
fn is_next_chunk_output(content: &str) -> bool {
    // Shape: "[chunk {start}..{end} of {total} chars, {pct}% through file]\n..."
    // Also matches the bare "EOF" signal only loosely — we DON'T want to
    // stub that since the agent might need it repeatedly. EOF has no
    // payload worth dropping anyway.
    content.starts_with("[chunk ") && content.contains(" of ") && content.contains(" chars,")
}

fn count_notes(working_dir: &Path) -> usize {
    std::fs::read_dir(working_dir)
        .map(|iter| iter.filter_map(|e| e.ok()).count())
        .unwrap_or(0)
}

/// Validate a note name: no path separators, no parent-dir escapes,
/// non-empty. Returns the full path on success.
fn validate_note_path(working_dir: &Path, name: &str) -> Result<PathBuf, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("note name cannot be empty".to_string());
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains("..") {
        return Err(format!(
            "note name must be a single filename (no /, \\, or ..): got {:?}",
            trimmed
        ));
    }
    if trimmed.starts_with('.') {
        return Err(format!(
            "note name cannot start with '.' (dotfiles disallowed): got {:?}",
            trimmed
        ));
    }
    Ok(working_dir.join(trimmed))
}

// ─── Sub-tool: next_chunk ───────────────────────────────────────────────

struct NextChunkTool {
    state: ReaderStateRef,
}

#[async_trait]
impl Tool for NextChunkTool {
    fn name(&self) -> &str {
        "next_chunk"
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "next_chunk".to_string(),
            description: format!(
                "Read the next `size` chars of the input file at the cursor, advance \
                 the cursor, return the chunk. `size` default {}, range [{}, {}]. \
                 Returns 'EOF' when the file is fully read.\n\
                 \n\
                 IMPORTANT: the previous chunk's text is REPLACED with a short stub \
                 on your next tool call to keep context bounded. Before calling \
                 next_chunk again, save anything you want to keep via append_note \
                 or write_note. Calling next_chunk twice in a row without a note in \
                 between means the earlier chunk's content is lost from your context \
                 permanently (the cursor is sequential; you can't go back).",
                DEFAULT_CHUNK_CHARS, MIN_CHUNK_CHARS, MAX_CHUNK_CHARS
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "size": {
                        "type": "integer",
                        "description": format!(
                            "Chunk size in chars (default {}, clamped to [{}, {}])",
                            DEFAULT_CHUNK_CHARS, MIN_CHUNK_CHARS, MAX_CHUNK_CHARS
                        )
                    }
                }
            }),
        }
    }
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let size = input
            .get("size")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(MIN_CHUNK_CHARS, MAX_CHUNK_CHARS))
            .unwrap_or(DEFAULT_CHUNK_CHARS);

        let mut s = self.state.lock().unwrap();
        if s.cursor >= s.input_text.len() {
            return ToolOutput::success("EOF".to_string());
        }
        let start = s.cursor;
        let mut end = (start + size).min(s.input_text.len());
        // Respect char boundaries.
        while end > start && !s.input_text.is_char_boundary(end) {
            end -= 1;
        }
        let chunk: String = s.input_text[start..end].to_string();
        s.cursor = end;
        let total = s.input_text.len();
        let progress = if total == 0 {
            100
        } else {
            (s.cursor * 100 / total).min(100)
        };
        ToolOutput::success(format!(
            "[chunk {}..{} of {} chars, {}% through file]\n{}",
            start, end, total, progress, chunk
        ))
    }
}

// ─── Sub-tool: write_note ───────────────────────────────────────────────

struct WriteNoteTool {
    state: ReaderStateRef,
}

#[async_trait]
impl Tool for WriteNoteTool {
    fn name(&self) -> &str {
        "write_note"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_note".to_string(),
            description: "Write `content` to a file `name` in the working directory. \
                          Overwrites if exists. Name must be a single filename (no \
                          path separators, no '..', no leading '.')."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["name", "content"]
            }),
        }
    }
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let name = match input.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return ToolOutput::error("Missing parameter: name".to_string()),
        };
        let content = match input.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolOutput::error("Missing parameter: content".to_string()),
        };
        if content.len() > MAX_NOTE_CHARS {
            return ToolOutput::error(format!(
                "Note too large: {} chars > {} cap",
                content.len(),
                MAX_NOTE_CHARS
            ));
        }
        let s = self.state.lock().unwrap();
        let path = match validate_note_path(&s.working_dir, name) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        drop(s);
        match std::fs::write(&path, content) {
            Ok(()) => ToolOutput::success(format!("Wrote {} bytes to {}", content.len(), name)),
            Err(e) => ToolOutput::error(format!("write_note {:?}: {}", path, e)),
        }
    }
}

// ─── Sub-tool: append_note ──────────────────────────────────────────────

struct AppendNoteTool {
    state: ReaderStateRef,
}

#[async_trait]
impl Tool for AppendNoteTool {
    fn name(&self) -> &str {
        "append_note"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "append_note".to_string(),
            description: "Append `content` to a file `name` in the working directory. \
                          Creates the file if missing. A newline is inserted before the \
                          appended content when the existing file doesn't end in one."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["name", "content"]
            }),
        }
    }
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let name = match input.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return ToolOutput::error("Missing parameter: name".to_string()),
        };
        let content = match input.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolOutput::error("Missing parameter: content".to_string()),
        };
        let s = self.state.lock().unwrap();
        let path = match validate_note_path(&s.working_dir, name) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        drop(s);

        // Enforce the note-size cap on the cumulative size.
        let existing_len = std::fs::metadata(&path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        if existing_len + content.len() > MAX_NOTE_CHARS {
            return ToolOutput::error(format!(
                "Note would exceed cap: {} existing + {} new > {}",
                existing_len,
                content.len(),
                MAX_NOTE_CHARS
            ));
        }

        // Insert a newline if the existing file doesn't end in one.
        let needs_newline = if existing_len > 0 {
            match std::fs::read(&path) {
                Ok(bytes) => bytes.last().copied() != Some(b'\n'),
                Err(_) => false,
            }
        } else {
            false
        };

        use std::io::Write;
        let mut f = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => return ToolOutput::error(format!("append_note open {:?}: {}", path, e)),
        };
        if needs_newline {
            let _ = f.write_all(b"\n");
        }
        match f.write_all(content.as_bytes()) {
            Ok(()) => ToolOutput::success(format!("Appended {} bytes to {}", content.len(), name)),
            Err(e) => ToolOutput::error(format!("append_note write {:?}: {}", path, e)),
        }
    }
}

// ─── Sub-tool: list_notes ───────────────────────────────────────────────

struct ListNotesTool {
    state: ReaderStateRef,
}

#[async_trait]
impl Tool for ListNotesTool {
    fn name(&self) -> &str {
        "list_notes"
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_notes".to_string(),
            description: "List files in the working directory with their sizes in bytes."
                .to_string(),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }
    async fn execute(&self, _input: &serde_json::Value) -> ToolOutput {
        let s = self.state.lock().unwrap();
        let dir = s.working_dir.clone();
        drop(s);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => return ToolOutput::error(format!("read_dir {:?}: {}", dir, e)),
        };
        let mut items: Vec<(String, u64)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            items.push((name, size));
        }
        items.sort_by(|a, b| a.0.cmp(&b.0));
        if items.is_empty() {
            return ToolOutput::success("(no notes yet)".to_string());
        }
        let mut out = String::from("Notes in working directory:\n");
        for (name, size) in items {
            out.push_str(&format!("  {}  ({} bytes)\n", name, size));
        }
        ToolOutput::success(out)
    }
}

// ─── Sub-tool: read_note ────────────────────────────────────────────────

struct ReadNoteTool {
    state: ReaderStateRef,
}

#[async_trait]
impl Tool for ReadNoteTool {
    fn name(&self) -> &str {
        "read_note"
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_note".to_string(),
            description: format!(
                "Read a note file from the working directory. Content is capped at {} chars \
                 — for larger notes, use `bash head/tail/sed` to view portions.",
                MAX_READ_NOTE_CHARS
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }),
        }
    }
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let name = match input.get("name").and_then(|v| v.as_str()) {
            Some(n) => n,
            None => return ToolOutput::error("Missing parameter: name".to_string()),
        };
        let s = self.state.lock().unwrap();
        let path = match validate_note_path(&s.working_dir, name) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        drop(s);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("read_note {:?}: {}", path, e)),
        };
        let truncated = if content.len() > MAX_READ_NOTE_CHARS {
            let mut i = MAX_READ_NOTE_CHARS;
            while i > 0 && !content.is_char_boundary(i) {
                i -= 1;
            }
            format!(
                "{}\n[TRUNCATED — full note is {} bytes; use `bash` to view more]",
                &content[..i],
                content.len()
            )
        } else {
            content
        };
        ToolOutput::success(truncated)
    }
}

// ─── Sub-tool: bash ─────────────────────────────────────────────────────

struct BashTool {
    state: ReaderStateRef,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description: format!(
                "Run a shell command with cwd set to the working directory. Useful for \
                 grep/sed/wc/cat on accumulated notes. Default timeout {}s (override \
                 per-call with `timeout_secs`, max 600s). Combined stdout+stderr \
                 capped at {} chars.",
                BASH_TIMEOUT_SECS, MAX_READ_NOTE_CHARS
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Wall-clock timeout in seconds (default 30, max 600)."
                    }
                },
                "required": ["command"]
            }),
        }
    }
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let command = match input.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.trim().is_empty() => c.to_string(),
            _ => return ToolOutput::error("Missing or empty parameter: command".to_string()),
        };
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .map(|n| n.clamp(1, 600))
            .unwrap_or(BASH_TIMEOUT_SECS);
        let s = self.state.lock().unwrap();
        let cwd = s.working_dir.clone();
        drop(s);
        // Wrap in `timeout` to enforce the budget. Same pattern as the
        // main bash tool uses for runaway commands.
        let output = Command::new("timeout")
            .arg(format!("{}s", timeout_secs))
            .arg("bash")
            .arg("-c")
            .arg(&command)
            .current_dir(&cwd)
            .output();
        let output = match output {
            Ok(o) => o,
            Err(e) => return ToolOutput::error(format!("bash exec: {}", e)),
        };
        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
        if !output.stderr.is_empty() {
            combined.push_str("\n--- stderr ---\n");
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        if combined.len() > MAX_READ_NOTE_CHARS {
            let mut i = MAX_READ_NOTE_CHARS;
            while i > 0 && !combined.is_char_boundary(i) {
                i -= 1;
            }
            combined.truncate(i);
            combined.push_str("\n[TRUNCATED]");
        }
        if !output.status.success() {
            return ToolOutput::error(format!(
                "bash exit {}: {}",
                output.status.code().unwrap_or(-1),
                combined
            ));
        }
        if combined.trim().is_empty() {
            ToolOutput::success("(no output)".to_string())
        } else {
            ToolOutput::success(combined)
        }
    }
}

// ─── Sub-tool: finish ───────────────────────────────────────────────────

struct FinishTool {
    state: ReaderStateRef,
}

#[async_trait]
impl Tool for FinishTool {
    fn name(&self) -> &str {
        "finish"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "finish".to_string(),
            description: "Terminate the reader with a final `result` string. The outer \
                          caller will see this along with the path to the working directory."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "result": {"type": "string"}
                },
                "required": ["result"]
            }),
        }
    }
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let result = match input.get("result").and_then(|v| v.as_str()) {
            Some(r) if !r.trim().is_empty() => r.trim().to_string(),
            _ => return ToolOutput::error("finish requires non-empty 'result'".to_string()),
        };
        let mut s = self.state.lock().unwrap();
        s.final_result = Some(result);
        ToolOutput::success("Reader finished.".to_string())
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
    fn auto_size_scales_with_content() {
        // Tiny file: floor clamp fires.
        assert_eq!(auto_size_max_turns(0), MIN_AUTO_TURNS);
        assert_eq!(auto_size_max_turns(500), MIN_AUTO_TURNS);

        // 20kb file (~3 chunks of 8k): 3 * 1.3 = 3.9 → 4, but
        // floor is 10, so we get 10.
        assert_eq!(auto_size_max_turns(20_000), MIN_AUTO_TURNS);

        // 100kb file: 100_000 / 8_000 = 13 chunks; 13 * 1.3 = 16.9
        // → 17. Above the floor, under the ceiling.
        assert_eq!(auto_size_max_turns(100_000), 17);

        // 1MB file: 128 chunks * 1.3 = 166.4 → 167.
        assert_eq!(auto_size_max_turns(1_024_000), 167);

        // Huge file (5MB): would compute >600 but ceiling clamps.
        assert_eq!(auto_size_max_turns(5_000_000), MAX_ALLOWED_TURNS);
    }

    #[test]
    fn auto_size_matches_docstring_examples() {
        // From the design discussion: a 17kb file should fit 1-2
        // chunks and need a handful of turns — floor applies.
        assert_eq!(auto_size_max_turns(17_000), MIN_AUTO_TURNS);

        // A 500kb file should need roughly 80-90 turns (63 chunks
        // * 1.3 ≈ 82). Well below the 200 ceiling.
        let big = auto_size_max_turns(500_000);
        assert!(
            (75..=95).contains(&big),
            "500kb auto-size {} should be in 75..=95",
            big
        );
    }

    fn fresh_state(text: &str, dir: &Path) -> ReaderStateRef {
        Arc::new(Mutex::new(ReaderState {
            input_text: text.to_string(),
            cursor: 0,
            working_dir: dir.to_path_buf(),
            final_result: None,
        }))
    }

    #[test]
    fn slug_from_path_basic() {
        assert_eq!(slug_from_path("/a/b/foo.rs"), "foo-rs");
        assert_eq!(slug_from_path("/tmp/bar.txt"), "bar-txt");
        assert_eq!(slug_from_path(""), "input");
    }

    #[test]
    fn slug_from_path_caps_at_40() {
        let long = "x".repeat(100);
        assert_eq!(slug_from_path(&long).len(), 40);
    }

    #[test]
    fn slug_from_path_collapses_dashes() {
        assert_eq!(
            slug_from_path("/some!@#path/with-many--non-ascii.ext"),
            "with-many-non-ascii-ext"
        );
    }

    #[test]
    fn validate_note_path_accepts_plain_names() {
        let dir = std::env::temp_dir();
        assert!(validate_note_path(&dir, "notes.md").is_ok());
        assert!(validate_note_path(&dir, "chapter_01.md").is_ok());
    }

    #[test]
    fn validate_note_path_rejects_escapes() {
        let dir = std::env::temp_dir();
        assert!(validate_note_path(&dir, "../outside").is_err());
        assert!(validate_note_path(&dir, "sub/file").is_err());
        assert!(validate_note_path(&dir, "").is_err());
        assert!(validate_note_path(&dir, "   ").is_err());
        assert!(validate_note_path(&dir, ".hidden").is_err());
    }

    #[tokio::test]
    async fn next_chunk_advances_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fresh_state("abcdefghij", tmp.path());
        let tool = NextChunkTool {
            state: state.clone(),
        };
        // size=3 → returns "abc", cursor=3
        let out = tool.execute(&json!({"size": 512})).await;
        assert!(!out.is_error);
        assert!(out.content.contains("abcdefghij"));
        assert_eq!(state.lock().unwrap().cursor, 10);
        // Next call → EOF
        let out2 = tool.execute(&json!({})).await;
        assert!(out2.content.contains("EOF"));
    }

    #[tokio::test]
    async fn next_chunk_respects_char_boundaries() {
        // Multi-byte char at position that would split
        let text = "abc😀defg"; // emoji is 4 bytes
        let tmp = tempfile::tempdir().unwrap();
        let state = fresh_state(text, tmp.path());
        let tool = NextChunkTool { state };
        // size=4 would land mid-emoji; tool should back off to char boundary
        let out = tool.execute(&json!({"size": 4})).await;
        assert!(!out.is_error);
        // Should have returned just "abc" (cursor moved to 3, the start of the emoji)
        assert!(out.content.contains("abc"));
        // If we accidentally split the emoji, the output would contain a
        // replacement character or be invalid UTF-8; since we returned
        // a String successfully, char boundary was respected.
    }

    #[tokio::test]
    async fn write_note_and_read_note_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fresh_state("", tmp.path());
        let writer = WriteNoteTool {
            state: state.clone(),
        };
        let reader = ReadNoteTool {
            state: state.clone(),
        };
        let w = writer
            .execute(&json!({"name": "notes.md", "content": "hello world"}))
            .await;
        assert!(!w.is_error);
        let r = reader.execute(&json!({"name": "notes.md"})).await;
        assert!(!r.is_error);
        assert_eq!(r.content, "hello world");
    }

    #[tokio::test]
    async fn append_note_inserts_newline_between_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fresh_state("", tmp.path());
        let append = AppendNoteTool {
            state: state.clone(),
        };
        let a = append
            .execute(&json!({"name": "log.txt", "content": "line A"}))
            .await;
        assert!(!a.is_error);
        let b = append
            .execute(&json!({"name": "log.txt", "content": "line B"}))
            .await;
        assert!(!b.is_error);
        let contents = std::fs::read_to_string(tmp.path().join("log.txt")).unwrap();
        assert_eq!(contents, "line A\nline B");
    }

    #[tokio::test]
    async fn write_note_rejects_large_content() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fresh_state("", tmp.path());
        let writer = WriteNoteTool { state };
        let huge = "x".repeat(MAX_NOTE_CHARS + 1);
        let out = writer
            .execute(&json!({"name": "huge.txt", "content": huge}))
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("too large"));
    }

    #[tokio::test]
    async fn list_notes_shows_what_was_written() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fresh_state("", tmp.path());
        let writer = WriteNoteTool {
            state: state.clone(),
        };
        let lister = ListNotesTool {
            state: state.clone(),
        };
        writer
            .execute(&json!({"name": "a.md", "content": "aa"}))
            .await;
        writer
            .execute(&json!({"name": "b.md", "content": "bbbb"}))
            .await;
        let out = lister.execute(&json!({})).await;
        assert!(!out.is_error);
        assert!(out.content.contains("a.md"));
        assert!(out.content.contains("b.md"));
        assert!(out.content.contains("2 bytes"));
        assert!(out.content.contains("4 bytes"));
    }

    #[test]
    fn compact_stubs_all_but_last_next_chunk_result() {
        let mut messages = vec![
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "id1".into(),
                    content: "[chunk 0..8000 of 100000 chars, 8% through file]\n<lots of text 1>"
                        .into(),
                    is_error: false,
                }],
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "thinking".into(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "id2".into(),
                    content:
                        "[chunk 8000..16000 of 100000 chars, 16% through file]\n<lots of text 2>"
                            .into(),
                    is_error: false,
                }],
            },
        ];
        compact_next_chunk_results(&mut messages);
        let first = match &messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => content,
            _ => panic!("expected tool result"),
        };
        assert!(
            first.contains("full text dropped"),
            "first should be stubbed: {}",
            first
        );
        assert!(first.contains("[chunk 0..8000"));
        assert!(!first.contains("<lots of text 1>"));
        let last = match &messages[2].content[0] {
            ContentBlock::ToolResult { content, .. } => content,
            _ => panic!("expected tool result"),
        };
        assert!(
            last.contains("<lots of text 2>"),
            "newest should be kept: {}",
            last
        );
    }

    #[test]
    fn compact_leaves_non_next_chunk_results_alone() {
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "id1".into(),
                content: "Wrote 42 bytes to findings.md".into(),
                is_error: false,
            }],
        }];
        compact_next_chunk_results(&mut messages);
        match &messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, "Wrote 42 bytes to findings.md");
            }
            _ => panic!("expected tool result"),
        }
    }

    #[test]
    fn compact_does_not_touch_eof_stubs() {
        // EOF tool_result doesn't start with "[chunk" so it shouldn't match.
        let mut messages = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "id1".into(),
                content: "EOF".into(),
                is_error: false,
            }],
        }];
        compact_next_chunk_results(&mut messages);
        match &messages[0].content[0] {
            ContentBlock::ToolResult { content, .. } => {
                assert_eq!(content, "EOF");
            }
            _ => panic!("expected tool result"),
        }
    }

    #[tokio::test]
    async fn finish_stores_result() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fresh_state("", tmp.path());
        let tool = FinishTool {
            state: state.clone(),
        };
        let out = tool.execute(&json!({"result": "the answer is 42"})).await;
        assert!(!out.is_error);
        assert_eq!(
            state.lock().unwrap().final_result,
            Some("the answer is 42".to_string())
        );
    }

    #[tokio::test]
    async fn finish_rejects_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let state = fresh_state("", tmp.path());
        let tool = FinishTool { state };
        let out = tool.execute(&json!({"result": ""})).await;
        assert!(out.is_error);
    }
}
