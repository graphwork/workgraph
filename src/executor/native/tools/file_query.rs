//! File-query backend: single-shot LLM query over a file's contents.
//!
//! Backend for `read_file(path, query=...)`. Runs ONE LLM call with the
//! file in the prompt and returns a text answer. If the file doesn't
//! fit in a single call, this returns an error pointing at the
//! `reader` tool — no silent cursor-loop fallback.
//!
//! Rationale (from the 2026-04-16 design exchange on tool boundaries):
//! small models learn from loud failures, not from silent magic. A
//! single `read_file` that sometimes runs a hidden multi-turn agent
//! loop is harder to reason about than two tools with clear
//! signatures. See `docs/design/unified-path-forward.md`.
//!
//! This file used to contain the cursor-traversal loop with
//! `read_chunk`/`note`/`finish` sub-tools (the former `survey_file`).
//! That logic moved into the `reader` tool as an internal mechanism
//! where it can have a working directory, sequential pull, and
//! persistent artifacts — all the things that make the heavy case
//! actually work.

use crate::executor::native::client::{ContentBlock, Message, MessagesRequest, Role};
use crate::executor::native::provider::Provider;

/// Fraction of context window budgeted for file contents in a single-shot
/// query. Leaves headroom for system prompt, the question, and the
/// model's response. Above this, we refuse and direct to `reader`.
const SINGLE_SHOT_CONTEXT_FRACTION: f64 = 0.6;

/// Conservative chars-per-token ratio for budget conversion. English
/// prose averages ~4 chars/token; code + markup is denser. 3 is a
/// conservative default that doesn't over-commit context.
const CHARS_PER_TOKEN: f64 = 3.0;

/// Public entry point: answer `query` about `path`'s contents.
///
/// Called from `ReadFileTool::execute` when a `query` parameter is
/// present. Single-shot only — errors out if the slice doesn't fit.
pub(crate) async fn run_query_on_file(
    workgraph_dir: &std::path::Path,
    path: &str,
    query: &str,
    offset: Option<usize>,
    limit: Option<usize>,
) -> Result<String, String> {
    // Resolve provider via the same chain research and deep_research use:
    // WG_MODEL env var > config.resolve_model_for_role(TaskAgent).
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

    let full_text = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read file '{}': {}", path, e))?;
    let text = apply_line_slice(&full_text, offset, limit);

    if text.trim().is_empty() {
        return Ok(format!(
            "File '{}' slice is empty (offset={:?}, limit={:?}). Cannot answer: {}",
            path, offset, limit, query
        ));
    }

    let budget =
        (provider.context_window() as f64 * SINGLE_SHOT_CONTEXT_FRACTION * CHARS_PER_TOKEN) as usize;
    if text.len() > budget {
        return Err(format!(
            "File slice is too large for single-shot query mode: {} chars > {} char budget \
             (context window: {} tokens). Either: (a) narrow the slice with `offset` and \
             `limit` to the relevant section, or (b) use the `reader` tool — it spawns a \
             sub-executor with a working directory that traverses arbitrarily large files \
             chunk by chunk.",
            text.len(),
            budget,
            provider.context_window()
        ));
    }

    run_single_shot_query(provider.as_ref(), path, query, &text).await
}

/// Apply a 1-based line `offset` and `limit` to `text`. Returns the
/// full text when neither is set.
fn apply_line_slice(text: &str, offset: Option<usize>, limit: Option<usize>) -> String {
    if offset.is_none() && limit.is_none() {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let start = offset.map(|o| o.saturating_sub(1)).unwrap_or(0).min(lines.len());
    let end = limit
        .map(|l| (start + l).min(lines.len()))
        .unwrap_or(lines.len());
    lines[start..end].join("\n")
}

/// Fire one LLM call with the full (sliced) file in the prompt and
/// return the answer.
async fn run_single_shot_query(
    provider: &dyn Provider,
    path: &str,
    query: &str,
    text: &str,
) -> Result<String, String> {
    eprintln!(
        "[file_query] single-shot on '{}' ({} chars): {:?}",
        path,
        text.len(),
        truncate(query, 80)
    );
    let prompt = format!(
        "You are answering a question from the contents of a file. Use ONLY the file \
         contents provided — do not fabricate. If the answer isn't in the file, say so \
         explicitly.\n\
         \n\
         File path: {}\n\
         \n\
         Question: {}\n\
         \n\
         --- File contents ---\n\
         {}\n\
         --- End of file ---\n\
         \n\
         Answer:",
        path, query, text
    );
    let request = MessagesRequest {
        model: provider.model().to_string(),
        max_tokens: provider.max_tokens(),
        system: Some(
            "You answer questions from file contents. Cite specific lines or passages \
             where relevant. Never fabricate — if the answer isn't in the provided text, \
             say so explicitly."
                .to_string(),
        ),
        messages: vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: prompt }],
        }],
        tools: vec![],
        stream: false,
    };
    let response = provider
        .send(&request)
        .await
        .map_err(|e| format!("single-shot API error: {}", e))?;
    let answer: String = response
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    if answer.trim().is_empty() {
        Err("empty single-shot response".to_string())
    } else {
        Ok(answer)
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
    fn line_slice_no_args_returns_full_text() {
        let text = "line1\nline2\nline3";
        assert_eq!(apply_line_slice(text, None, None), text);
    }

    #[test]
    fn line_slice_offset_only() {
        let text = "one\ntwo\nthree\nfour";
        // 1-based offset
        assert_eq!(apply_line_slice(text, Some(2), None), "two\nthree\nfour");
    }

    #[test]
    fn line_slice_limit_only() {
        let text = "one\ntwo\nthree\nfour";
        assert_eq!(apply_line_slice(text, None, Some(2)), "one\ntwo");
    }

    #[test]
    fn line_slice_offset_and_limit() {
        let text = "one\ntwo\nthree\nfour\nfive";
        assert_eq!(apply_line_slice(text, Some(2), Some(2)), "two\nthree");
    }

    #[test]
    fn line_slice_offset_beyond_end_is_empty() {
        let text = "one\ntwo";
        assert_eq!(apply_line_slice(text, Some(10), None), "");
    }

    #[test]
    fn line_slice_offset_zero_treated_as_1_based_with_saturating_sub() {
        let text = "one\ntwo";
        // offset=0 → saturating_sub(1)=0 → same as offset=1
        assert_eq!(apply_line_slice(text, Some(0), None), "one\ntwo");
    }
}
