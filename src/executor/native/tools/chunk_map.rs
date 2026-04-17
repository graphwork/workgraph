//! chunk_map: split a big file (or inline text) into chunks and run a
//! `map` sub-agent over each chunk.
//!
//! The missing primitive between `summarize` and `map`:
//!
//! - `summarize(path, instruction)` → one aggregated text summary via
//!   map-reduce over chunks. Single-shot LLM per chunk, no tools.
//! - `map(inputs, task)` → sub-agent per item with working dir, expects
//!   the caller to already have a list of items.
//! - `chunk_map(path, task)` → splits the file into chunks and runs
//!   map's sub-agent pipeline over each chunk. Per-chunk working dirs,
//!   per-chunk `finish(result)`, aggregated `results.md`.
//!
//! Use when a target file is too large for the model's context but the
//! per-chunk task needs tool access (notes, bash, ...) — not just a
//! single-shot summarization. Concrete example: "list every named
//! character in this 150KB novel" — each chunk sub-agent extracts names
//! from its slice, aggregation dedupes.
//!
//! Shape:
//!
//!   chunk_map(path OR text, task) → same return as map()
//!
//! Produces:
//!
//!   <workgraph>/maps/<timestamp-slug>/
//!     results.md          — aggregated per-chunk finish() results
//!     items/
//!       00-chunk-0000/    — sub-executor dir for chunk 0
//!       01-chunk-0001/    — sub-executor dir for chunk 1
//!       ...
//!
//! Chunking strategy: same as `summarize`'s `chunk_text` — paragraph
//! boundaries first, then character boundary fallback. Chunk size
//! defaults to 40% of the provider's context window (matching
//! `summarize`), capped by `max_bytes_per_chunk` if supplied.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::json;

use super::{Tool, ToolOutput, ToolRegistry};
use crate::executor::native::client::ToolDefinition;

/// Default upper bound on chunks per call. 100 chunks × 20 turns ×
/// per-turn cost is already plenty; larger splits suggest the caller
/// wants a batch job, not an agent tool.
const MAX_CHUNKS: usize = 100;

/// Default max turns per chunk. Matches `map`'s default.
const DEFAULT_MAX_TURNS_PER_ITEM: usize = 20;

/// Hard cap on per-chunk turns, matches `map`.
const MAX_ALLOWED_TURNS: usize = 100;

/// Default wall-clock ceiling per chunk. Matches `map`'s default.
const DEFAULT_TIMEOUT_SECS_PER_ITEM: u64 = 180;

/// Hard cap on per-chunk timeout. Matches `map`'s cap.
const MAX_TIMEOUT_SECS_PER_ITEM: u64 = 20 * 60;

/// Chunks smaller than this are still fine — the minimum exists only
/// to protect against pathological caller-supplied values.
const MIN_BYTES_PER_CHUNK: usize = 512;

pub fn register_chunk_map_tool(registry: &mut ToolRegistry, workgraph_dir: PathBuf) {
    registry.register(Box::new(ChunkMapTool { workgraph_dir }));
}

struct ChunkMapTool {
    workgraph_dir: PathBuf,
}

#[async_trait]
impl Tool for ChunkMapTool {
    fn name(&self) -> &str {
        "chunk_map"
    }

    fn is_read_only(&self) -> bool {
        // Writes only into its own working dirs under `<workgraph>/maps/`.
        // Same convention as `map` + `reader`.
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "chunk_map".to_string(),
            description: "Split a file (or inline text) into chunks and run a sub-agent over \
                          each chunk with the same task. Each chunk gets its own working dir \
                          with notes + bash + finish. Returns a parent working dir containing \
                          per-chunk results and aggregated results.md.\n\
                          \n\
                          Use when a target is too large for the model's context AND the \
                          per-chunk task needs tools (notes, bash) — not just single-shot \
                          summarization (for that, use `summarize`).\n\
                          \n\
                          Compare:\n\
                          - `summarize(path, instruction)` → one merged summary, no tools\n\
                          - `reader(path, task)`           → multi-turn cursor traversal, one result\n\
                          - `map(inputs, task)`            → sub-agent per pre-chunked item\n\
                          - `chunk_map(path, task)`        → auto-chunk then map\n\
                          \n\
                          Chunks respect paragraph boundaries where possible. Default chunk \
                          size is ~40% of the model's context window (leaving room for \
                          system prompt + sub-agent notes + finish)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the file to chunk. One of `path` or `text` is required."
                    },
                    "text": {
                        "type": "string",
                        "description": "Inline text to chunk. One of `path` or `text` is required. For large content prefer `path` so the full body stays out of the request."
                    },
                    "task": {
                        "type": "string",
                        "description": "The instruction applied to every chunk. Be specific about the output format — the same task string is used for every chunk so downstream aggregation is easier when outputs follow a shared shape. E.g. 'list every named character mentioned in this chunk, one per line'."
                    },
                    "max_bytes_per_chunk": {
                        "type": "integer",
                        "description": format!(
                            "Override for max chunk size in bytes. Default = 40% of the \
                             model's context window (≈12k chars on a 32k model). Min {}.",
                            MIN_BYTES_PER_CHUNK
                        )
                    },
                    "max_turns_per_item": {
                        "type": "integer",
                        "description": format!(
                            "Max conversation turns per chunk sub-agent (default {}, cap {}). \
                             Cost ceiling.",
                            DEFAULT_MAX_TURNS_PER_ITEM, MAX_ALLOWED_TURNS
                        )
                    },
                    "timeout_secs_per_item": {
                        "type": "integer",
                        "description": format!(
                            "Wall-clock ceiling per chunk in seconds (default {}, cap {}). \
                             Whichever fires first (turns or time) kills the sub-agent.",
                            DEFAULT_TIMEOUT_SECS_PER_ITEM, MAX_TIMEOUT_SECS_PER_ITEM
                        )
                    }
                },
                "required": ["task"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let path = input.get("path").and_then(|v| v.as_str());
        let inline = input.get("text").and_then(|v| v.as_str());
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

        let (source_label, body) = match (path, inline) {
            (Some(p), _) if !p.is_empty() => {
                let body = match std::fs::read_to_string(p) {
                    Ok(b) => b,
                    Err(e) => {
                        return ToolOutput::error(format!("read {}: {}", p, e));
                    }
                };
                (format!("path={}", p), body)
            }
            (_, Some(t)) if !t.is_empty() => ("inline text".to_string(), t.to_string()),
            _ => {
                return ToolOutput::error(
                    "Provide one of `path` or `text`.".to_string(),
                );
            }
        };

        if body.is_empty() {
            return ToolOutput::error(format!("{} is empty — nothing to chunk", source_label));
        }

        // Resolve the provider that `map` will use so we can pick a
        // chunk size appropriate to its window.
        let config = crate::config::Config::load_or_default(&self.workgraph_dir);
        let model = std::env::var("WG_MODEL")
            .ok()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| {
                config
                    .resolve_model_for_role(crate::config::DispatchRole::TaskAgent)
                    .model
            });
        let provider = match crate::executor::native::provider::create_provider(
            &self.workgraph_dir,
            &model,
        ) {
            Ok(p) => p,
            Err(e) => {
                return ToolOutput::error(format!("resolve provider: {}", e));
            }
        };

        let default_chunk_bytes =
            super::summarize::chunk_size_chars(provider.context_window());
        let chunk_bytes = input
            .get("max_bytes_per_chunk")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).max(MIN_BYTES_PER_CHUNK))
            .unwrap_or(default_chunk_bytes);

        let raw_chunks = super::summarize::chunk_text(&body, chunk_bytes);
        if raw_chunks.is_empty() {
            return ToolOutput::error(
                "chunk_text produced zero chunks — nothing to process".to_string(),
            );
        }
        if raw_chunks.len() > MAX_CHUNKS {
            return ToolOutput::error(format!(
                "Content split into {} chunks at {} bytes/chunk, max {}. Raise \
                 max_bytes_per_chunk or process a smaller slice of the source.",
                raw_chunks.len(),
                chunk_bytes,
                MAX_CHUNKS
            ));
        }

        let total_chunks = raw_chunks.len();
        let total_bytes = body.len();
        eprintln!(
            "\x1b[2m[chunk_map] source={}, {} bytes → {} chunk(s) @ ~{} bytes/chunk (model window {})\x1b[0m",
            source_label,
            total_bytes,
            total_chunks,
            chunk_bytes,
            provider.context_window(),
        );

        // Build the per-chunk `inputs` payload for map. Prepend each chunk
        // with a provenance header so the sub-agent knows which slice of
        // the source it's working on — important when aggregating results
        // across chunks with overlapping content.
        let mut byte_cursor: usize = 0;
        let mut inputs: Vec<String> = Vec::with_capacity(total_chunks);
        for (i, chunk) in raw_chunks.iter().enumerate() {
            let start = byte_cursor;
            let end = byte_cursor + chunk.len();
            byte_cursor = end;
            inputs.push(format!(
                "[Chunk {} of {} — bytes {}..{} of {} from {}]\n\n{}",
                i + 1,
                total_chunks,
                start,
                end,
                total_bytes,
                source_label,
                chunk
            ));
        }

        // Delegate to `map`'s sub-agent machinery. Reuses its working-dir
        // layout, per-item compaction, aggregation.
        match super::map::run_map(
            &self.workgraph_dir,
            &inputs,
            &task,
            max_turns_per_item,
            timeout_secs_per_item,
        )
        .await
        {
            Ok(result) => ToolOutput::success(result),
            Err(e) => ToolOutput::error(format!("chunk_map failed: {}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_tool(temp: &std::path::Path) -> ChunkMapTool {
        ChunkMapTool {
            workgraph_dir: temp.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn missing_task_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let tool = test_tool(dir.path());
        let out = tool.execute(&json!({"text": "hello"})).await;
        assert!(out.is_error);
        assert!(out.content.to_lowercase().contains("task"));
    }

    #[tokio::test]
    async fn missing_source_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let tool = test_tool(dir.path());
        let out = tool
            .execute(&json!({"task": "do a thing"}))
            .await;
        assert!(out.is_error);
        assert!(out.content.to_lowercase().contains("path"));
    }

    #[tokio::test]
    async fn empty_text_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let tool = test_tool(dir.path());
        let out = tool
            .execute(&json!({"task": "x", "text": ""}))
            .await;
        assert!(out.is_error);
        // Either "empty" or "Provide one of" — both are acceptable rejections.
        let s = out.content.to_lowercase();
        assert!(s.contains("empty") || s.contains("provide one of"));
    }

    #[tokio::test]
    async fn nonexistent_path_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let tool = test_tool(dir.path());
        let out = tool
            .execute(&json!({
                "task": "x",
                "path": "/nonexistent/file/nope.txt"
            }))
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("read"));
    }
}
