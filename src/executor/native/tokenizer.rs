//! Tokenizer-aware token counting for `ContextBudget`.
//!
//! Replaces the `chars / 4` heuristic with a real tokenizer loaded
//! from `tiktoken-rs` (bundled in-binary, no network, no Python).
//! Different model families map to different tokenizer tables; in
//! practice, cl100k_base and o200k_base cover everything we
//! reasonably hit.
//!
//! The 4-chars-per-token heuristic systematically undercounts for
//! code (closer to 3.0-3.3 chars/token). That makes compaction
//! pressure fire late on small-context models — by the time we say
//! "compact at next boundary," the next turn has already blown the
//! window. Real counts fix this.
//!
//! Tokenizer loads are expensive (10-50ms); we cache per-family in
//! a `OnceLock`. Load failure falls back to `chars / 4` with a
//! single warn-log — never panic, never break the session.

use std::sync::{Arc, OnceLock};

use tiktoken_rs::CoreBPE;

/// Shared instance of the cl100k_base tokenizer. Used for most
/// models (Anthropic family, Qwen, Gemini approximation, older
/// GPT).
static CL100K: OnceLock<Option<Arc<CoreBPE>>> = OnceLock::new();

/// Shared instance of the o200k_base tokenizer. Used for modern
/// OpenAI models (gpt-4o, gpt-4.1, o1, o3, o4).
static O200K: OnceLock<Option<Arc<CoreBPE>>> = OnceLock::new();

/// Best-effort tokenizer lookup by model id. Returns `None` if
/// loading failed (rare — cl100k is bundled); callers fall back to
/// the char-based heuristic.
pub fn tokenizer_for_model(model: &str) -> Option<Arc<CoreBPE>> {
    let lower = model.to_lowercase();
    if uses_o200k(&lower) {
        O200K
            .get_or_init(|| match tiktoken_rs::o200k_base() {
                Ok(bpe) => Some(Arc::new(bpe)),
                Err(e) => {
                    eprintln!("[tokenizer] o200k_base load failed: {} — falling back", e);
                    None
                }
            })
            .clone()
    } else {
        CL100K
            .get_or_init(|| match tiktoken_rs::cl100k_base() {
                Ok(bpe) => Some(Arc::new(bpe)),
                Err(e) => {
                    eprintln!("[tokenizer] cl100k_base load failed: {} — falling back", e);
                    None
                }
            })
            .clone()
    }
}

/// Modern OpenAI models use `o200k_base`. Everything else (Anthropic,
/// older GPT, Qwen, Gemini, local) falls back to `cl100k_base` —
/// approximation, but within ~5%, which is good enough for context-
/// pressure decisions.
fn uses_o200k(lower_model: &str) -> bool {
    lower_model.contains("gpt-4o")
        || lower_model.contains("gpt-4.1")
        || lower_model.contains("gpt-5")
        || lower_model.contains("o1-")
        || lower_model.contains("o1:")
        || lower_model.contains("/o1")
        || lower_model.contains("o3-")
        || lower_model.contains("o3:")
        || lower_model.contains("/o3")
        || lower_model.contains("o4-")
        || lower_model.contains("o4:")
        || lower_model.contains("/o4")
}

/// Count tokens for `text` under `model`'s tokenizer. Falls back to
/// `text.len() / 4` if the tokenizer isn't available.
pub fn count_tokens(text: &str, model: &str) -> usize {
    match tokenizer_for_model(model) {
        Some(bpe) => bpe.encode_with_special_tokens(text).len(),
        None => text.len() / 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cl100k_loads_and_counts() {
        let count = count_tokens("hello world", "claude-haiku-4-latest");
        // Real count should be > 0 and much smaller than `len / 4`
        // heuristic for a short English phrase (~2 tokens).
        assert!(count > 0);
        assert!(count < 10);
    }

    #[test]
    fn o200k_chosen_for_openai_modern_models() {
        assert!(uses_o200k("openai:gpt-4o"));
        assert!(uses_o200k("openai/gpt-4.1-mini"));
        assert!(uses_o200k("o3-mini"));
        assert!(!uses_o200k("claude-sonnet-4-latest"));
        assert!(!uses_o200k("oai-compat:qwen3-coder-30b"));
    }

    #[test]
    fn code_heavy_text_counts_more_than_chars_over_four() {
        // A dense snippet of Rust source: real tokenization produces
        // more tokens than chars/4 suggests.
        let rust = "pub fn foo(x: i32) -> Result<Vec<Option<&str>>, anyhow::Error> {\n    let y: HashMap<String, Vec<u8>> = HashMap::new();\n    Ok(vec![])\n}";
        let real = count_tokens(rust, "claude-sonnet-4-latest");
        let heuristic = rust.len() / 4;
        // Real should be at least 15% more than the heuristic for
        // code — the whole point of replacing chars/4.
        assert!(
            real as f64 > heuristic as f64 * 1.15,
            "real={} vs heuristic={} (expected real ≥ 1.15 × heuristic)",
            real,
            heuristic,
        );
    }

    #[test]
    fn empty_string_counts_zero() {
        assert_eq!(count_tokens("", "any-model"), 0);
    }
}
