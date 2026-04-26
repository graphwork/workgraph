//! Chat task ID formatting and parsing.
//!
//! New chat agents use the `.chat-N` prefix. Legacy graphs may contain
//! `.coordinator-N` tasks; lookups accept both prefixes for one release.
//! Use `wg migrate chat-rename` to rewrite legacy IDs.

use crate::graph::WorkGraph;

pub const CHAT_PREFIX: &str = ".chat-";
pub const LEGACY_COORDINATOR_PREFIX: &str = ".coordinator-";

pub const CHAT_LOOP_TAG: &str = "chat-loop";
pub const LEGACY_COORDINATOR_LOOP_TAG: &str = "coordinator-loop";

/// Format a task ID for a new chat agent (`.chat-<N>`).
pub fn format_chat_task_id(id: u32) -> String {
    format!("{}{}", CHAT_PREFIX, id)
}

/// Parse a chat task ID (accepts both `.chat-N` and legacy `.coordinator-N`).
pub fn parse_chat_task_id(s: &str) -> Option<u32> {
    if let Some(rest) = s.strip_prefix(CHAT_PREFIX) {
        rest.parse().ok()
    } else if let Some(rest) = s.strip_prefix(LEGACY_COORDINATOR_PREFIX) {
        rest.parse().ok()
    } else {
        None
    }
}

/// Returns true if this task ID identifies a chat agent (either prefix).
pub fn is_chat_task_id(s: &str) -> bool {
    s.starts_with(CHAT_PREFIX) || s.starts_with(LEGACY_COORDINATOR_PREFIX)
}

/// Look up a chat task by numeric ID, trying `.chat-N` first then `.coordinator-N`.
pub fn find_chat_task<'g>(
    graph: &'g WorkGraph,
    id: u32,
) -> Option<&'g crate::graph::Task> {
    let new_id = format_chat_task_id(id);
    if let Some(t) = graph.get_task(&new_id) {
        return Some(t);
    }
    let legacy_id = format!("{}{}", LEGACY_COORDINATOR_PREFIX, id);
    graph.get_task(&legacy_id)
}

/// Returns the canonical task ID string for a chat agent in this graph,
/// preferring an existing legacy `.coordinator-N` record so we don't accidentally
/// double-create. New IDs use `.chat-N`.
pub fn canonical_chat_task_id(graph: &WorkGraph, id: u32) -> String {
    let new_id = format_chat_task_id(id);
    if graph.get_task(&new_id).is_some() {
        return new_id;
    }
    let legacy_id = format!("{}{}", LEGACY_COORDINATOR_PREFIX, id);
    if graph.get_task(&legacy_id).is_some() {
        return legacy_id;
    }
    new_id
}

/// Returns true if this tag marks a chat agent loop (either new or legacy form).
pub fn is_chat_loop_tag(tag: &str) -> bool {
    tag == CHAT_LOOP_TAG || tag == LEGACY_COORDINATOR_LOOP_TAG
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_new_prefix() {
        assert_eq!(format_chat_task_id(0), ".chat-0");
        assert_eq!(format_chat_task_id(7), ".chat-7");
    }

    #[test]
    fn parses_both_prefixes() {
        assert_eq!(parse_chat_task_id(".chat-0"), Some(0));
        assert_eq!(parse_chat_task_id(".chat-42"), Some(42));
        assert_eq!(parse_chat_task_id(".coordinator-3"), Some(3));
        assert_eq!(parse_chat_task_id(".coordinator-99"), Some(99));
        assert_eq!(parse_chat_task_id("not-a-chat"), None);
        assert_eq!(parse_chat_task_id(".chat-abc"), None);
    }

    #[test]
    fn detects_chat_id() {
        assert!(is_chat_task_id(".chat-0"));
        assert!(is_chat_task_id(".coordinator-1"));
        assert!(!is_chat_task_id(".compact-0"));
        assert!(!is_chat_task_id("regular-task"));
    }

    #[test]
    fn detects_loop_tag() {
        assert!(is_chat_loop_tag("chat-loop"));
        assert!(is_chat_loop_tag("coordinator-loop"));
        assert!(!is_chat_loop_tag("compact-loop"));
    }
}
