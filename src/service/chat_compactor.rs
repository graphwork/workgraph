//! Chat compactor: summarizes per-coordinator conversation history into context-summary.md.
//!
//! Distinct from the graph-level compactor (`compactor.rs`), this operates on
//! the chat history (inbox + outbox) for a single coordinator.
//!
//! Produces `.workgraph/chat/<coordinator-id>/context-summary.md` with:
//! - Key decisions made (scaled to model context window)
//! - Open threads / pending items (scaled to model context window)
//! - User preferences expressed (scaled to model context window)
//! - Recurring topics (scaled to model context window)
//!
//! Supports incremental compaction: each run builds on the previous summary
//! plus new messages since the last compaction.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

use crate::chat::{self, ChatMessage};
use crate::config::{Config, DispatchRole};

/// Directory for a coordinator's chat files.
fn chat_dir_for(workgraph_dir: &Path, coordinator_id: u32) -> PathBuf {
    workgraph_dir.join("chat").join(coordinator_id.to_string())
}

/// Path to the generated context-summary.md for a coordinator.
pub fn context_summary_path(workgraph_dir: &Path, coordinator_id: u32) -> PathBuf {
    chat_dir_for(workgraph_dir, coordinator_id).join("context-summary.md")
}

/// Path to the chat compactor state file for a coordinator.
fn state_path(workgraph_dir: &Path, coordinator_id: u32) -> PathBuf {
    chat_dir_for(workgraph_dir, coordinator_id).join("compactor-state.json")
}

/// Persistent state for chat compaction.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatCompactorState {
    /// Timestamp of the last compaction.
    pub last_compaction: Option<String>,
    /// Number of messages that were included in the last compaction.
    pub last_message_count: usize,
    /// Total number of compactions performed.
    pub compaction_count: u64,
    /// ID of the last inbox message included in previous compaction.
    pub last_inbox_id: u64,
    /// ID of the last outbox message included in previous compaction.
    pub last_outbox_id: u64,
}

impl ChatCompactorState {
    pub fn load(workgraph_dir: &Path, coordinator_id: u32) -> Self {
        let path = state_path(workgraph_dir, coordinator_id);
        if path.exists() {
            fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self, workgraph_dir: &Path, coordinator_id: u32) -> Result<()> {
        let dir = chat_dir_for(workgraph_dir, coordinator_id);
        fs::create_dir_all(&dir)?;
        let json = serde_json::to_string_pretty(self)?;
        fs::write(state_path(workgraph_dir, coordinator_id), json)?;
        Ok(())
    }
}

/// Check whether chat compaction should run based on message count since last compaction.
///
/// Returns true if the number of new messages exceeds a threshold (default: 50).
pub fn should_compact(workgraph_dir: &Path, coordinator_id: u32) -> bool {
    let state = ChatCompactorState::load(workgraph_dir, coordinator_id);
    let new_inbox = chat::read_inbox_since_for(workgraph_dir, coordinator_id, state.last_inbox_id)
        .unwrap_or_default();
    let new_outbox =
        chat::read_outbox_since_for(workgraph_dir, coordinator_id, state.last_outbox_id)
            .unwrap_or_default();

    let new_count = new_inbox.len() + new_outbox.len();
    let config = Config::load_or_default(workgraph_dir);
    let threshold = config.chat.compact_threshold;

    new_count >= threshold
}

/// Resolve the context window size for the chat compactor's model.
///
/// Resolution order: model registry entry → endpoint config → 200k default.
fn resolve_chat_compactor_context_window(config: &Config) -> u64 {
    let resolved = config.resolve_model_for_role(DispatchRole::ChatCompactor);
    if let Some(ref entry) = resolved.registry_entry {
        if entry.context_window > 0 {
            return entry.context_window;
        }
    }
    if let Some(ref ep_name) = resolved.endpoint {
        if let Some(ep) = config.llm_endpoints.find_by_name(ep_name) {
            if let Some(cw) = ep.context_window {
                return cw;
            }
        }
    }
    200_000
}

/// Compute section token budgets for the chat compactor prompt based on the context window.
///
/// Uses 2% of the context window, clamped to [400, 4000], split 42/25/17/17 across
/// key-decisions / open-threads / user-preferences / recurring-topics.
fn chat_compactor_section_budgets(context_window: u64) -> (u64, u64, u64, u64) {
    let total_budget = (context_window as f64 * 0.02).round() as u64;
    let total_budget = total_budget.clamp(400, 4000);
    let decisions = (total_budget as f64 * 0.42).round() as u64;
    let threads = (total_budget as f64 * 0.25).round() as u64;
    let preferences = (total_budget as f64 * 0.17).round() as u64;
    let recurring = total_budget
        .saturating_sub(decisions)
        .saturating_sub(threads)
        .saturating_sub(preferences);
    (
        decisions.max(150),
        threads.max(100),
        preferences.max(75),
        recurring.max(75),
    )
}

/// Run chat compaction for a specific coordinator.
///
/// Reads all chat history (or new messages since last compaction for incremental mode),
/// calls the LLM to produce a summary, and writes context-summary.md.
pub fn run_chat_compaction(workgraph_dir: &Path, coordinator_id: u32) -> Result<PathBuf> {
    let config = Config::load_or_default(workgraph_dir);
    let state = ChatCompactorState::load(workgraph_dir, coordinator_id);

    // Read current context-summary if it exists (for incremental compaction)
    let output_path = context_summary_path(workgraph_dir, coordinator_id);
    let previous_summary = if output_path.exists() {
        fs::read_to_string(&output_path).unwrap_or_default()
    } else {
        String::new()
    };

    // Read new messages since last compaction
    let new_inbox = chat::read_inbox_since_for(workgraph_dir, coordinator_id, state.last_inbox_id)?;
    let new_outbox =
        chat::read_outbox_since_for(workgraph_dir, coordinator_id, state.last_outbox_id)?;

    // If no new messages, nothing to compact
    if new_inbox.is_empty() && new_outbox.is_empty() {
        if output_path.exists() {
            return Ok(output_path);
        }
        anyhow::bail!(
            "No chat messages to compact for coordinator {}",
            coordinator_id
        );
    }

    // Interleave by timestamp
    let mut new_messages = new_inbox.clone();
    new_messages.extend(new_outbox.clone());
    new_messages.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    // Resolve context window for dynamic budget scaling
    let context_window = resolve_chat_compactor_context_window(&config);

    // Build the prompt
    let prompt = build_chat_compactor_prompt(&previous_summary, &new_messages, context_window);

    // Call the LLM
    let result = super::llm::run_lightweight_llm_call(
        &config,
        DispatchRole::ChatCompactor,
        &prompt,
        120, // 2 minute timeout
    )
    .context("Chat compactor LLM call failed")?;

    // Write context-summary.md
    let dir = chat_dir_for(workgraph_dir, coordinator_id);
    fs::create_dir_all(&dir)?;
    fs::write(&output_path, &result.text)?;

    // Track the max IDs we've now compacted
    let max_inbox_id = new_inbox
        .iter()
        .map(|m| m.id)
        .max()
        .unwrap_or(state.last_inbox_id);
    let max_outbox_id = new_outbox
        .iter()
        .map(|m| m.id)
        .max()
        .unwrap_or(state.last_outbox_id);

    // Update state
    let new_state = ChatCompactorState {
        last_compaction: Some(Utc::now().to_rfc3339()),
        last_message_count: new_messages.len(),
        compaction_count: state.compaction_count + 1,
        last_inbox_id: max_inbox_id,
        last_outbox_id: max_outbox_id,
    };
    new_state.save(workgraph_dir, coordinator_id)?;

    Ok(output_path)
}

/// Format chat messages into a compact text representation for the prompt.
fn format_messages_for_prompt(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for msg in messages {
        // Truncate very long messages to keep prompt manageable
        let content = if msg.content.len() > 500 {
            format!(
                "{}...",
                &msg.content[..msg.content.floor_char_boundary(500)]
            )
        } else {
            msg.content.clone()
        };
        let time = if let Some(t_pos) = msg.timestamp.find('T') {
            let time_part = &msg.timestamp[t_pos + 1..];
            if time_part.len() >= 8 {
                &time_part[..8]
            } else {
                time_part
            }
        } else {
            &msg.timestamp
        };
        out.push_str(&format!("[{}] {}: {}\n", time, msg.role, content));
    }
    out
}

/// Build the LLM prompt for chat compaction.
fn build_chat_compactor_prompt(
    previous_summary: &str,
    new_messages: &[ChatMessage],
    context_window: u64,
) -> String {
    let mut prompt = String::from(
        "You are a conversation compactor for a workgraph coordinator. \
         Your job is to produce a concise context summary that captures the essential \
         state of the conversation so far.\n\n",
    );

    if !previous_summary.is_empty() {
        prompt.push_str("## Previous Summary\n\n");
        prompt.push_str(previous_summary);
        prompt.push_str("\n\n## New Messages Since Last Compaction\n\n");
    } else {
        prompt.push_str("## Conversation Messages\n\n");
    }

    // Cap messages to avoid oversized prompts
    let messages_to_include = if new_messages.len() > 200 {
        &new_messages[new_messages.len() - 200..]
    } else {
        new_messages
    };

    prompt.push_str(&format_messages_for_prompt(messages_to_include));

    let (decisions_budget, threads_budget, prefs_budget, recurring_budget) =
        chat_compactor_section_budgets(context_window);

    prompt.push_str(&format!(
        "\n\n## Output Format\n\n\
         Produce a markdown document with EXACTLY these four sections. \
         The document should be self-contained — a coordinator reading only this \
         document should be able to resume the conversation without losing context.\n\n\
         ### 1. Key Decisions (~{} tokens)\n\
         Bullet list of decisions made during the conversation: what was agreed, \
         what approach was chosen, what was rejected and why. Include task IDs and \
         specific details where relevant.\n\n\
         ### 2. Open Threads (~{} tokens)\n\
         Items that are still in progress or unresolved: pending questions, \
         tasks that were discussed but not completed, topics that need follow-up.\n\n\
         ### 3. User Preferences (~{} tokens)\n\
         Communication style, tool preferences, workflow habits, or explicit \
         instructions the user has given about how they want to work.\n\n\
         ### 4. Recurring Topics (~{} tokens)\n\
         Themes, goals, or concerns that come up repeatedly. Patterns in what \
         the user asks about or works on.\n\n\
         IMPORTANT: Output ONLY the context summary document. No preamble, no explanation. \
         Start directly with '# Conversation Context Summary'. \
         If building on a previous summary, incorporate its content — do not lose information. \
         Update or revise previous entries as needed based on new messages.",
        decisions_budget, threads_budget, prefs_budget, recurring_budget,
    ));

    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();
        (tmp, wg_dir)
    }

    #[test]
    fn test_chat_compactor_state_roundtrip() {
        let (_tmp, dir) = setup();

        let state = ChatCompactorState {
            last_compaction: Some("2026-03-27T12:00:00Z".to_string()),
            last_message_count: 10,
            compaction_count: 3,
            last_inbox_id: 5,
            last_outbox_id: 4,
        };
        state.save(&dir, 0).unwrap();

        let loaded = ChatCompactorState::load(&dir, 0);
        assert_eq!(loaded.last_message_count, 10);
        assert_eq!(loaded.compaction_count, 3);
        assert_eq!(loaded.last_inbox_id, 5);
        assert_eq!(loaded.last_outbox_id, 4);
    }

    #[test]
    fn test_chat_compactor_state_default_on_missing() {
        let (_tmp, dir) = setup();
        let state = ChatCompactorState::load(&dir, 0);
        assert!(state.last_compaction.is_none());
        assert_eq!(state.last_message_count, 0);
        assert_eq!(state.compaction_count, 0);
        assert_eq!(state.last_inbox_id, 0);
        assert_eq!(state.last_outbox_id, 0);
    }

    #[test]
    fn test_context_summary_path() {
        let path = context_summary_path(Path::new("/tmp/wg"), 0);
        assert_eq!(path, PathBuf::from("/tmp/wg/chat/0/context-summary.md"));
    }

    #[test]
    fn test_context_summary_path_nonzero_coordinator() {
        let path = context_summary_path(Path::new("/tmp/wg"), 3);
        assert_eq!(path, PathBuf::from("/tmp/wg/chat/3/context-summary.md"));
    }

    #[test]
    fn test_format_messages_for_prompt() {
        let messages = vec![
            ChatMessage {
                id: 1,
                timestamp: "2026-03-27T10:00:00Z".to_string(),
                role: "user".to_string(),
                content: "Hello, let's work on the task".to_string(),
                request_id: "req-1".to_string(),
                attachments: vec![],
                full_response: None,
                user: None,
            },
            ChatMessage {
                id: 1,
                timestamp: "2026-03-27T10:00:05Z".to_string(),
                role: "coordinator".to_string(),
                content: "Sure, I'll create the tasks now".to_string(),
                request_id: "req-1".to_string(),
                attachments: vec![],
                full_response: None,
                user: None,
            },
        ];

        let formatted = format_messages_for_prompt(&messages);
        assert!(formatted.contains("[10:00:00] user: Hello"));
        assert!(formatted.contains("[10:00:05] coordinator: Sure"));
    }

    #[test]
    fn test_build_prompt_without_previous_summary() {
        let messages = vec![ChatMessage {
            id: 1,
            timestamp: "2026-03-27T10:00:00Z".to_string(),
            role: "user".to_string(),
            content: "test message".to_string(),
            request_id: "req-1".to_string(),
            attachments: vec![],
            full_response: None,
            user: None,
        }];

        let prompt = build_chat_compactor_prompt("", &messages, 200_000);
        assert!(prompt.contains("## Conversation Messages"));
        assert!(!prompt.contains("## Previous Summary"));
        assert!(prompt.contains("Key Decisions"));
        assert!(prompt.contains("Open Threads"));
        assert!(prompt.contains("User Preferences"));
        assert!(prompt.contains("Recurring Topics"));
    }

    #[test]
    fn test_build_prompt_with_previous_summary() {
        let messages = vec![ChatMessage {
            id: 1,
            timestamp: "2026-03-27T10:00:00Z".to_string(),
            role: "user".to_string(),
            content: "test message".to_string(),
            request_id: "req-1".to_string(),
            attachments: vec![],
            full_response: None,
            user: None,
        }];

        let prompt = build_chat_compactor_prompt(
            "# Previous context\nSome decisions were made.",
            &messages,
            200_000,
        );
        assert!(prompt.contains("## Previous Summary"));
        assert!(prompt.contains("Previous context"));
        assert!(prompt.contains("## New Messages Since Last Compaction"));
    }

    #[test]
    fn test_should_compact_no_messages() {
        let (_tmp, dir) = setup();
        assert!(!should_compact(&dir, 0));
    }

    #[test]
    fn test_should_compact_below_threshold() {
        let (_tmp, dir) = setup();
        // Add a few messages (below default threshold of 50)
        for i in 0..5 {
            chat::append_inbox(&dir, &format!("msg {}", i), &format!("req-{}", i)).unwrap();
        }
        assert!(!should_compact(&dir, 0));
    }

    #[test]
    fn test_chat_compactor_budgets_default_200k() {
        let (decisions, threads, prefs, recurring) = chat_compactor_section_budgets(200_000);
        // 200k * 0.02 = 4000 (at the cap)
        assert_eq!(decisions + threads + prefs + recurring, 4000);
        assert!(decisions > threads);
    }

    #[test]
    fn test_chat_compactor_budgets_small_window() {
        let (decisions, threads, prefs, recurring) = chat_compactor_section_budgets(16_000);
        // 16k * 0.02 = 320 → clamped to 400, then section minimums apply
        assert!(decisions >= 150);
        assert!(threads >= 100);
        assert!(prefs >= 75);
        assert!(recurring >= 75);
        // Total may exceed 400 slightly due to section floor clamping
        let total = decisions + threads + prefs + recurring;
        assert!(total >= 400 && total <= 500);
    }

    #[test]
    fn test_chat_compactor_budgets_very_large_window() {
        let (decisions, threads, prefs, recurring) = chat_compactor_section_budgets(1_000_000);
        // 1M * 0.02 = 20000 → clamped to 4000
        assert_eq!(decisions + threads + prefs + recurring, 4000);
    }
}
