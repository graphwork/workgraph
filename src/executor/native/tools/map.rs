//! Map tool: apply an operation to each of N inputs with a sub-executor
//! per item, sequential execution, aggregated results.
//!
//! The missing primitive the unified-path-forward doc called out. Where
//! `reader` loops chunks over ONE file, `map` loops items over N things.
//! Same shape: each item gets its own mini-executor with a working
//! subdirectory, its own compaction, its own finish. The parent
//! working dir collects results + preserves each item's sub-dir for
//! later inspection.
//!
//! Shape:
//!
//!   map(inputs, task) → parent_working_dir
//!
//! Produces:
//!
//!   parent_working_dir/
//!     results.md           — aggregated per-item findings
//!     items/
//!       00-<slug>/         — sub-executor dir for inputs[0]
//!       01-<slug>/         — sub-executor dir for inputs[1]
//!       ...
//!
//! Sub-executor tool set (per item):
//!   - append_note, write_note, list_notes, read_note
//!   - bash (cwd = item's sub-dir)
//!   - finish(result)
//!
//! Sequential, not parallel. Concurrency would multiply rate-limit
//! pressure on the provider and complicate partial-failure handling;
//! if parallelism becomes necessary, a later version can add it with
//! an explicit concurrency cap.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolOutput, ToolRegistry};
use crate::executor::native::client::{
    ContentBlock, Message, MessagesRequest, Role, StopReason, ToolDefinition,
};

/// Default max turns per item. Each item's mini-executor gets this
/// many turns to complete its slice of the task.
const DEFAULT_MAX_TURNS_PER_ITEM: usize = 20;

/// Hard cap on max_turns_per_item — prevents runaway cost even if a
/// caller passes something absurd.
const MAX_ALLOWED_TURNS: usize = 100;

/// Hard cap on inputs count. A map over 10K items would be a sign
/// of misuse — that's a batch job, not an agent task.
const MAX_INPUTS: usize = 200;

/// Cap on any individual note in the sub-working-dir.
const MAX_NOTE_CHARS: usize = 1_024 * 1_024;

/// Cap on returned read_note / bash output to keep tool_result blocks
/// bounded.
const MAX_READ_OUTPUT_CHARS: usize = 40_000;

/// Timeout for a single bash invocation inside an item's mini-executor.
const BASH_TIMEOUT_SECS: u64 = 30;

/// Default wall-clock ceiling per item, independent of max_turns.
/// Protects against slow-model hangs and long-running sub-agent work
/// that max_turns can't cap (because N turns × slow model = unbounded
/// time). Whichever fires first kills the sub-agent and records a
/// timeout error for that item. 180s is generous enough for a
/// multi-turn summarize-plus-bash cycle on a mid-tier local model,
/// short enough that a genuinely-hung worker fails fast.
const DEFAULT_TIMEOUT_SECS_PER_ITEM: u64 = 180;

/// Hard cap on per-item timeout so a caller can't accidentally disable
/// it with an absurd value. 20 min should fit any realistic sub-agent
/// task; batch jobs go outside the agent loop.
const MAX_TIMEOUT_SECS_PER_ITEM: u64 = 20 * 60;

const MAP_ITEM_SYSTEM_PROMPT: &str = "\
You are a WORKER running as one of many sub-agents in a parallel `map` \
operation. A parent agent is coordinating you and others; your output \
will be concatenated verbatim with peers who received the same task \
instruction over different inputs.

## Your output must match the format your peers' will use

Read the task instruction in the initial message EXACTLY as written. If \
it asks for 'a numbered list', produce a numbered list — not prose, not \
a narrative, not a summary. If it asks for 'one paragraph', produce one \
paragraph. Deviation breaks downstream aggregation.

Do not add conversational framing ('Here are the results...', 'Based on \
my analysis...', 'I hope this helps...'). Emit only what was asked for.

## Your tools

  - append_note(name, content) — append to a file in your working dir
  - write_note(name, content)  — create/overwrite a file
  - list_notes()                — list files in your working dir
  - read_note(name)             — read back a note
  - bash(command)               — shell with cwd = your working dir
  - finish(result)              — terminate; `result` is what the parent sees

## The core rule

Your working directory is persistent. Your conversation context is NOT. \
Only `finish(result)` reaches the parent. Rich intermediate detail \
belongs in notes — they survive for later inspection but don't pollute \
the aggregated output.

Finish as soon as you have the answer. Extra turns after that are \
wasted work.";

pub fn register_map_tool(registry: &mut ToolRegistry, workgraph_dir: PathBuf) {
    registry.register(Box::new(MapTool { workgraph_dir }));
}

struct MapTool {
    workgraph_dir: PathBuf,
}

#[async_trait]
impl Tool for MapTool {
    fn name(&self) -> &str {
        "map"
    }

    fn is_read_only(&self) -> bool {
        // map writes only into its own working directory, not the
        // user's source tree — treated as read-only from the outer
        // perspective, same convention as reader.
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "map".to_string(),
            description: "Apply an operation to each of N inputs, sequentially, each with \
                          its own sub-executor and working subdirectory. Use this when the \
                          same task needs to run over a list of items — 'summarize each of \
                          these 10 URLs', 'extract X from each of these 30 files', 'ask \
                          this question of each of these 20 documents'. Each item gets its \
                          own persistent working dir where the sub-agent writes notes; the \
                          parent dir aggregates per-item results into a single results.md. \
                          Returns the parent working dir path and a summary of per-item \
                          results.\n\
                          \n\
                          Compare: reader = one resource, deep traversal. map = many \
                          resources, uniform operation. deep_research = decompose one \
                          question into sub-questions then synthesize."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "inputs": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": format!(
                            "List of input strings (file paths, URLs, queries — whatever \
                             the operation consumes). Max {} items.",
                            MAX_INPUTS
                        )
                    },
                    "task": {
                        "type": "string",
                        "description": "What the sub-agent should do with each input. Be \
                                        specific about the output format you want — the \
                                        same task string is used for every item."
                    },
                    "max_turns_per_item": {
                        "type": "integer",
                        "description": format!(
                            "Max conversation turns per item (default {}, cap {}). Each \
                             turn = one LLM call. Cost ceiling.",
                            DEFAULT_MAX_TURNS_PER_ITEM, MAX_ALLOWED_TURNS
                        )
                    },
                    "timeout_secs_per_item": {
                        "type": "integer",
                        "description": format!(
                            "Wall-clock ceiling per item in seconds (default {}, cap {}). \
                             Independent of max_turns — whichever fires first kills the \
                             sub-agent. Protects against slow-model hangs that max_turns \
                             can't cap.",
                            DEFAULT_TIMEOUT_SECS_PER_ITEM, MAX_TIMEOUT_SECS_PER_ITEM
                        )
                    }
                },
                "required": ["inputs", "task"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let inputs: Vec<String> = match input.get("inputs").and_then(|v| v.as_array()) {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect(),
            None => {
                return ToolOutput::error("Missing or non-array parameter: inputs".to_string());
            }
        };
        if inputs.is_empty() {
            return ToolOutput::error("inputs list cannot be empty".to_string());
        }
        if inputs.len() > MAX_INPUTS {
            return ToolOutput::error(format!(
                "inputs list has {} items, max is {}. For larger batches, run map in \
                 chunks or use a batch job outside the agent loop.",
                inputs.len(),
                MAX_INPUTS
            ));
        }
        let task = match input.get("task").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => return ToolOutput::error("Missing or empty parameter: task".to_string()),
        };
        let max_turns_per_item = input
            .get("max_turns_per_item")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, MAX_ALLOWED_TURNS))
            .unwrap_or(DEFAULT_MAX_TURNS_PER_ITEM);
        let timeout_secs_per_item = input
            .get("timeout_secs_per_item")
            .and_then(|v| v.as_u64())
            .map(|n| n.clamp(1, MAX_TIMEOUT_SECS_PER_ITEM))
            .unwrap_or(DEFAULT_TIMEOUT_SECS_PER_ITEM);

        match run_map(
            &self.workgraph_dir,
            &inputs,
            &task,
            max_turns_per_item,
            timeout_secs_per_item,
        )
        .await
        {
            Ok(result) => ToolOutput::success(result),
            Err(e) => ToolOutput::error(format!("map failed: {}", e)),
        }
    }
}

/// Main map loop. Creates the parent working dir, iterates over inputs,
/// spawns a mini-executor per item, aggregates results.
///
/// Exposed as `pub(crate)` so `chunk_map` (which auto-chunks a file
/// before fan-out) can delegate here without duplicating the sub-agent
/// + working-dir machinery.
pub(crate) async fn run_map(
    workgraph_dir: &Path,
    inputs: &[String],
    task: &str,
    max_turns_per_item: usize,
    timeout_secs_per_item: u64,
) -> Result<String, String> {
    let parent_dir = make_parent_dir(workgraph_dir, task)?;
    let items_dir = parent_dir.join("items");
    std::fs::create_dir_all(&items_dir)
        .map_err(|e| format!("create items dir {:?}: {}", items_dir, e))?;

    eprintln!(
        "[map] start: {} items, task={:?}, parent_dir={}",
        inputs.len(),
        truncate(task, 80),
        parent_dir.display()
    );

    // Resolve provider once for reuse across items.
    let config = crate::config::Config::load_or_default(workgraph_dir);
    let model = std::env::var("WG_MODEL")
        .ok()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| {
            config
                .resolve_model_for_role(crate::config::DispatchRole::TaskAgent)
                .model
        });

    let mut per_item_results: Vec<(usize, String, Result<String, String>)> = Vec::new();
    for (i, item_input) in inputs.iter().enumerate() {
        let item_slug = item_slug_from_input(item_input, i);
        let item_dir = items_dir.join(&item_slug);
        std::fs::create_dir_all(&item_dir)
            .map_err(|e| format!("create item_dir {:?}: {}", item_dir, e))?;
        eprintln!(
            "[map] item {}/{} ({}): {}",
            i + 1,
            inputs.len(),
            item_slug,
            truncate(item_input, 80)
        );
        // Fresh provider per item — keeps the conversations truly
        // independent even if the underlying client has caches.
        let provider =
            match crate::executor::native::provider::create_provider(workgraph_dir, &model) {
                Ok(p) => p,
                Err(e) => {
                    per_item_results.push((
                        i,
                        item_input.clone(),
                        Err(format!("create provider: {}", e)),
                    ));
                    continue;
                }
            };
        let item_label = format!("{}/{}", i + 1, inputs.len());
        let outcome = match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs_per_item),
            run_item(
                provider.as_ref(),
                &item_dir,
                item_input,
                task,
                max_turns_per_item,
                &item_label,
            ),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => {
                eprintln!(
                    "\x1b[33m[map] item {}/{} timed out after {}s — killing sub-agent\x1b[0m",
                    i + 1,
                    inputs.len(),
                    timeout_secs_per_item
                );
                Err(format!(
                    "item timed out after {}s (wall-clock ceiling). Whatever the \
                     sub-agent had written to its working dir is preserved at {}.",
                    timeout_secs_per_item,
                    item_dir.display()
                ))
            }
        };
        per_item_results.push((i, item_input.clone(), outcome));
    }

    // Aggregate results into parent_dir/results.md.
    let results_md = aggregate_results(task, &per_item_results);
    let results_path = parent_dir.join("results.md");
    std::fs::write(&results_path, &results_md).map_err(|e| format!("write results.md: {}", e))?;

    let (ok, fail) =
        per_item_results.iter().fold(
            (0, 0),
            |(o, f), (_, _, r)| {
                if r.is_ok() { (o + 1, f) } else { (o, f + 1) }
            },
        );

    Ok(format!(
        "Map result: {} of {} items completed ({} failed).\n\
         Parent working directory: {}\n\
         Aggregated results: {}\n\
         Per-item sub-dirs: {}/<NN-slug>/\n\
         \n\
         Inspect per-item findings with `bash cat {}` or `bash ls {}` for \
         the full workspace. Each sub-dir contains the sub-agent's notes \
         and artifacts for that input.",
        ok,
        inputs.len(),
        fail,
        parent_dir.display(),
        results_path.display(),
        items_dir.display(),
        results_path.display(),
        parent_dir.display(),
    ))
}

/// Run the mini-executor for a single input. Returns the sub-agent's
/// finish(result) on success, or an error describing why we gave up.
///
/// `item_label` is a human-readable "N/M" string used in per-turn
/// telemetry so the outer user can see progress inside an item's
/// sub-agent loop (otherwise a 20-turn item looks hung).
async fn run_item(
    provider: &dyn crate::executor::native::provider::Provider,
    working_dir: &Path,
    item_input: &str,
    task: &str,
    max_turns: usize,
    item_label: &str,
) -> Result<String, String> {
    let state = Arc::new(Mutex::new(MapItemState {
        working_dir: working_dir.to_path_buf(),
        final_result: None,
    }));

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(AppendNoteTool {
        state: state.clone(),
    }));
    registry.register(Box::new(WriteNoteTool {
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
        "Task: {}\n\nInput: {}\n\nWrite notes as you work, finish with a concise result \
         when done.",
        task, item_input
    );
    let mut messages = vec![Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: initial_msg }],
    }];

    for turn in 0..max_turns {
        let request = MessagesRequest {
            model: provider.model().to_string(),
            max_tokens: provider.max_tokens(),
            system: Some(MAP_ITEM_SYSTEM_PROMPT.to_string()),
            messages: messages.clone(),
            tools: tool_defs.clone(),
            stream: false,
        };
        let response = provider
            .send(&request)
            .await
            .map_err(|e| format!("API error turn {}: {}", turn + 1, e))?;
        messages.push(Message {
            role: Role::Assistant,
            content: response.content.clone(),
        });

        match response.stop_reason {
            Some(StopReason::EndTurn) | Some(StopReason::StopSequence) | None => {
                let s = state.lock().unwrap();
                if let Some(ref r) = s.final_result {
                    return Ok(r.clone());
                }
                drop(s);
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Use a tool. When you have your answer, call finish(result)."
                            .to_string(),
                    }],
                });
                continue;
            }
            Some(StopReason::MaxTokens) => {
                messages.push(Message {
                    role: Role::User,
                    content: vec![ContentBlock::Text {
                        text: "Response truncated. Call finish with a concise answer from \
                               notes so far."
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
                // Per-turn progress line so the outer user sees work
                // happening inside an item's sub-agent. Otherwise a
                // long-running item looks frozen.
                let tool_names: Vec<&str> = tool_uses.iter().map(|(_, n, _)| n.as_str()).collect();
                eprintln!(
                    "\x1b[2m[map item {} turn {}/{}: {}]\x1b[0m",
                    item_label,
                    turn + 1,
                    max_turns,
                    tool_names.join("+")
                );
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
                let s = state.lock().unwrap();
                if let Some(ref r) = s.final_result {
                    return Ok(r.clone());
                }
            }
        }
    }

    // No finish within budget. Return whatever we have (maybe nothing)
    // as an error so the aggregator can record the failure.
    let s = state.lock().unwrap();
    match s.final_result.clone() {
        Some(r) => Ok(r),
        None => Err(format!(
            "item reached max turns ({}) without finish",
            max_turns
        )),
    }
}

/// Create the parent working dir at
/// `<workgraph_dir>/maps/<timestamp>-<task-slug>/`.
fn make_parent_dir(workgraph_dir: &Path, task: &str) -> Result<PathBuf, String> {
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
    let slug = task_slug(task);
    let dir = workgraph_dir
        .join("maps")
        .join(format!("{}-{}", stamp, slug));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {:?}: {}", dir, e))?;
    Ok(dir)
}

/// Slug from a task string: alphanumerics + dashes, capped at 40 chars.
fn task_slug(task: &str) -> String {
    let mut out: String = task
        .chars()
        .take(60)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out = out.trim_matches('-').to_string();
    if out.is_empty() {
        out = "task".to_string();
    }
    if out.len() > 40 {
        out.truncate(40);
    }
    out
}

/// Slug for an item's sub-dir: `NN-<slug>`, 2-digit zero-padded index
/// + a slug derived from the input (short and safe).
fn item_slug_from_input(input: &str, index: usize) -> String {
    let slug = task_slug(input);
    let short = if slug.len() > 30 {
        &slug[..30]
    } else {
        slug.as_str()
    };
    format!("{:02}-{}", index, short)
}

/// Build the aggregated results.md from per-item outcomes.
fn aggregate_results(task: &str, items: &[(usize, String, Result<String, String>)]) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Map results: {}\n\n", task));
    s.push_str(&format!("Total items: {}\n\n", items.len()));
    for (i, input, outcome) in items {
        s.push_str(&format!("## [{:02}] {}\n\n", i, truncate(input, 200)));
        match outcome {
            Ok(r) => {
                s.push_str(r);
                s.push_str("\n\n");
            }
            Err(e) => {
                s.push_str(&format!("**ERROR:** {}\n\n", e));
            }
        }
    }
    s
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

// ─── Sub-tools (per-item mini-executor) ─────────────────────────────────
//
// Near-duplicates of reader.rs's equivalents with MapItemState instead
// of ReaderState. Factoring this into a shared trait would remove ~100
// lines of duplication; deferred until both tools stabilize and we
// know exactly which shape generalizes.

struct MapItemState {
    working_dir: PathBuf,
    final_result: Option<String>,
}

type MapItemStateRef = Arc<Mutex<MapItemState>>;

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

struct WriteNoteTool {
    state: MapItemStateRef,
}

#[async_trait]
impl Tool for WriteNoteTool {
    fn name(&self) -> &str {
        "write_note"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "write_note".to_string(),
            description: "Write content to a file `name` in your working dir (overwrites)."
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
                "Note too large: {} > {}",
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
            Err(e) => ToolOutput::error(format!("write {:?}: {}", path, e)),
        }
    }
}

struct AppendNoteTool {
    state: MapItemStateRef,
}

#[async_trait]
impl Tool for AppendNoteTool {
    fn name(&self) -> &str {
        "append_note"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "append_note".to_string(),
            description:
                "Append content to a file `name` in your working dir (creates if missing)."
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
        let existing_len = std::fs::metadata(&path)
            .map(|m| m.len() as usize)
            .unwrap_or(0);
        if existing_len + content.len() > MAX_NOTE_CHARS {
            return ToolOutput::error(format!(
                "Note would exceed cap: {} + {} > {}",
                existing_len,
                content.len(),
                MAX_NOTE_CHARS
            ));
        }
        let needs_newline = if existing_len > 0 {
            std::fs::read(&path)
                .map(|b| b.last().copied() != Some(b'\n'))
                .unwrap_or(false)
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
            Err(e) => return ToolOutput::error(format!("open {:?}: {}", path, e)),
        };
        if needs_newline {
            let _ = f.write_all(b"\n");
        }
        match f.write_all(content.as_bytes()) {
            Ok(()) => ToolOutput::success(format!("Appended {} bytes to {}", content.len(), name)),
            Err(e) => ToolOutput::error(format!("write {:?}: {}", path, e)),
        }
    }
}

struct ListNotesTool {
    state: MapItemStateRef,
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
            description: "List files in your working dir with sizes.".to_string(),
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
        let mut out = String::from("Notes:\n");
        for (name, size) in items {
            out.push_str(&format!("  {} ({} bytes)\n", name, size));
        }
        ToolOutput::success(out)
    }
}

struct ReadNoteTool {
    state: MapItemStateRef,
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
                "Read a note from your working dir. Output capped at {} chars.",
                MAX_READ_OUTPUT_CHARS
            ),
            input_schema: json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
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
            Err(e) => return ToolOutput::error(format!("read {:?}: {}", path, e)),
        };
        let out = if content.len() > MAX_READ_OUTPUT_CHARS {
            let mut i = MAX_READ_OUTPUT_CHARS;
            while i > 0 && !content.is_char_boundary(i) {
                i -= 1;
            }
            format!(
                "{}\n[TRUNCATED — full note is {} bytes]",
                &content[..i],
                content.len()
            )
        } else {
            content
        };
        ToolOutput::success(out)
    }
}

struct BashTool {
    state: MapItemStateRef,
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
                "Run a shell command with cwd = your working dir. Default timeout \
                 {}s (override per-call with `timeout_secs`, max 600s). Output \
                 capped at {} chars.",
                BASH_TIMEOUT_SECS, MAX_READ_OUTPUT_CHARS
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout_secs": {
                        "type": "integer",
                        "description": "Wall-clock timeout in seconds (default 30, max 600). Raise for long-running commands like builds or test suites."
                    }
                },
                "required": ["command"]
            }),
        }
    }
    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let command = match input.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.trim().is_empty() => c.to_string(),
            _ => return ToolOutput::error("Missing or empty command".to_string()),
        };
        // Per-call timeout override, clamped to [1s, 600s]. Matches the
        // claude-code-ts pattern: default 30s stays conservative, but
        // agents running `cargo build` or `cargo test` can request more.
        let timeout_secs = input
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .map(|n| n.clamp(1, 600))
            .unwrap_or(BASH_TIMEOUT_SECS);
        let s = self.state.lock().unwrap();
        let cwd = s.working_dir.clone();
        drop(s);
        // Cross-platform `timeout(1)` replacement — Windows has no equivalent.
        let output = crate::platform_timeout::spawn_with_timeout(
            "bash",
            |cmd| {
                cmd.arg("-c")
                    .arg(&command)
                    .current_dir(&cwd)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
            },
            timeout_secs,
        )
        .and_then(|(child, _killer)| child.wait_with_output());
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
        if combined.len() > MAX_READ_OUTPUT_CHARS {
            let mut i = MAX_READ_OUTPUT_CHARS;
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

struct FinishTool {
    state: MapItemStateRef,
}

#[async_trait]
impl Tool for FinishTool {
    fn name(&self) -> &str {
        "finish"
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "finish".to_string(),
            description: "Terminate this item's processing with a concise `result` string. \
                          The parent map operation aggregates results across all items. \
                          Rich detail goes in notes, not the result string."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"result": {"type": "string"}},
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
        ToolOutput::success("Item finished.".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_slug_basic() {
        assert_eq!(task_slug("summarize each file"), "summarize-each-file");
        assert_eq!(
            task_slug("Extract X from the docs"),
            "extract-x-from-the-docs"
        );
    }

    #[test]
    fn task_slug_caps_at_40() {
        let long = "describe-a-".repeat(20);
        assert!(task_slug(&long).len() <= 40);
    }

    #[test]
    fn task_slug_empty_fallback() {
        assert_eq!(task_slug(""), "task");
        assert_eq!(task_slug("!@#$%"), "task");
    }

    #[test]
    fn item_slug_from_input_has_index_prefix() {
        assert!(item_slug_from_input("https://example.com", 0).starts_with("00-"));
        assert!(item_slug_from_input("https://example.com", 7).starts_with("07-"));
        assert!(item_slug_from_input("https://example.com", 42).starts_with("42-"));
    }

    #[test]
    fn aggregate_results_happy_path() {
        let items = vec![
            (0, "url1".to_string(), Ok("result 1".to_string())),
            (1, "url2".to_string(), Ok("result 2".to_string())),
        ];
        let out = aggregate_results("summarize", &items);
        assert!(out.contains("# Map results: summarize"));
        assert!(out.contains("Total items: 2"));
        assert!(out.contains("[00] url1"));
        assert!(out.contains("result 1"));
        assert!(out.contains("[01] url2"));
        assert!(out.contains("result 2"));
    }

    #[test]
    fn aggregate_results_records_errors() {
        let items = vec![
            (0, "ok-item".to_string(), Ok("fine".to_string())),
            (1, "bad-item".to_string(), Err("boom".to_string())),
        ];
        let out = aggregate_results("task", &items);
        assert!(out.contains("fine"));
        assert!(out.contains("**ERROR:** boom"));
    }

    #[test]
    fn validate_note_path_rejects_escapes() {
        let dir = std::env::temp_dir();
        assert!(validate_note_path(&dir, "ok.md").is_ok());
        assert!(validate_note_path(&dir, "../out").is_err());
        assert!(validate_note_path(&dir, "sub/nested").is_err());
        assert!(validate_note_path(&dir, ".hidden").is_err());
        assert!(validate_note_path(&dir, "").is_err());
    }

    #[tokio::test]
    async fn finish_stores_result() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(MapItemState {
            working_dir: tmp.path().to_path_buf(),
            final_result: None,
        }));
        let tool = FinishTool {
            state: state.clone(),
        };
        let out = tool.execute(&json!({"result": "done"})).await;
        assert!(!out.is_error);
        assert_eq!(state.lock().unwrap().final_result.as_deref(), Some("done"));
    }

    #[tokio::test]
    async fn finish_rejects_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(MapItemState {
            working_dir: tmp.path().to_path_buf(),
            final_result: None,
        }));
        let tool = FinishTool { state };
        let out = tool.execute(&json!({"result": ""})).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn write_and_read_note_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(Mutex::new(MapItemState {
            working_dir: tmp.path().to_path_buf(),
            final_result: None,
        }));
        let w = WriteNoteTool {
            state: state.clone(),
        };
        let r = ReadNoteTool {
            state: state.clone(),
        };
        w.execute(&json!({"name": "x.md", "content": "hi"})).await;
        let out = r.execute(&json!({"name": "x.md"})).await;
        assert_eq!(out.content, "hi");
    }
}
