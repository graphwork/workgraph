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

/// Returns true if this task ID is a legacy coordinator (`.coordinator-N` or bare `.coordinator`).
/// Use this to apply distinct visual treatment during the deprecation window.
pub fn is_legacy_coordinator_id(s: &str) -> bool {
    s.starts_with(LEGACY_COORDINATOR_PREFIX) || s == ".coordinator"
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

/// Tmux session-name prefix for chat-persistence wrappers. The orphan
/// sweep at TUI startup uses this exact prefix to find dangling
/// sessions whose backing chat task no longer exists.
pub const CHAT_TMUX_SESSION_PREFIX: &str = "wg-chat-";

/// Best-effort: kill the tmux session backing a given chat id. No-op
/// when tmux is not on PATH or the session doesn't exist. Used by every
/// chat-archive / chat-delete path so we don't accumulate orphan
/// sessions across the TUI / CLI / IPC archive surfaces.
///
/// Returns `true` iff a session was actually killed (useful for emitting
/// "Closed N tmux sessions" toasts; callers can ignore otherwise).
pub fn kill_chat_tmux_session_for_id(workgraph_dir: &std::path::Path, chat_id: u32) -> bool {
    let project_root = workgraph_dir
        .parent()
        .unwrap_or(workgraph_dir)
        .to_path_buf();
    let project_tag = project_root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let chat_ref = format!("chat-{}", chat_id);
    let session = chat_tmux_session_name(project_tag, &chat_ref);
    // Quick has-session probe: avoids spawning kill-session when there's
    // nothing there (so the no-op case is silent + cheap).
    let exists = std::process::Command::new("tmux")
        .args(["has-session", "-t", &session])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !exists {
        return false;
    }
    let _ = std::process::Command::new("tmux")
        .args(["kill-session", "-t", &session])
        .status();
    true
}

/// Build the canonical tmux session name for a chat. Format:
/// `wg-chat-<project_tag>-chat-<N>` (mirrors the existing
/// `wg-{project}` namespace from `wg server`). Caller passes the project
/// tag (typically the project root's basename).
///
/// `chat_ref` is the user-facing alias (e.g. "chat-0"); the function
/// is tolerant of `.chat-0` task ids too — leading dots are stripped so
/// the result is a valid tmux session name (no `.` or `:`).
pub fn chat_tmux_session_name(project_tag: &str, chat_ref: &str) -> String {
    let chat_ref = chat_ref.trim_start_matches('.');
    let project_tag = sanitize_session_segment(project_tag);
    format!("{}{}-{}", CHAT_TMUX_SESSION_PREFIX, project_tag, chat_ref)
}

/// Tmux session names cannot contain `:` or `.`. Project basenames in
/// the wild can include either (e.g. `wg.test`), so squash them to `-`.
fn sanitize_session_segment(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ':' | '.' => '-',
            c if c.is_whitespace() => '-',
            c => c,
        })
        .collect()
}

/// Parse a tmux session name produced by [`chat_tmux_session_name`] and
/// return the chat ref (`chat-N`) embedded in it. Returns `None` for
/// names that don't match the chat-tmux schema or whose project tag
/// doesn't match.
pub fn parse_chat_tmux_session(name: &str, project_tag: &str) -> Option<String> {
    let project_tag = sanitize_session_segment(project_tag);
    let prefix = format!("{}{}-", CHAT_TMUX_SESSION_PREFIX, project_tag);
    let rest = name.strip_prefix(&prefix)?;
    if rest.starts_with("chat-") && rest[5..].chars().all(|c| c.is_ascii_digit()) {
        Some(rest.to_string())
    } else {
        None
    }
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

    #[test]
    fn formats_tmux_session_name() {
        assert_eq!(
            chat_tmux_session_name("workgraph", "chat-0"),
            "wg-chat-workgraph-chat-0"
        );
        // Tolerates a `.chat-N` task id with the leading dot.
        assert_eq!(
            chat_tmux_session_name("workgraph", ".chat-3"),
            "wg-chat-workgraph-chat-3"
        );
        // Sanitizes : and . in the project tag.
        assert_eq!(
            chat_tmux_session_name("wg.test", "chat-7"),
            "wg-chat-wg-test-chat-7"
        );
    }

    #[test]
    fn parses_tmux_session_name() {
        assert_eq!(
            parse_chat_tmux_session("wg-chat-workgraph-chat-0", "workgraph"),
            Some("chat-0".to_string())
        );
        assert_eq!(
            parse_chat_tmux_session("wg-chat-workgraph-chat-99", "workgraph"),
            Some("chat-99".to_string())
        );
        // Wrong project tag — must not match.
        assert_eq!(
            parse_chat_tmux_session("wg-chat-other-chat-0", "workgraph"),
            None
        );
        // Non-chat suffix — must not match.
        assert_eq!(
            parse_chat_tmux_session("wg-chat-workgraph-server", "workgraph"),
            None
        );
        // Outer wg-tui session — must not match (no chat- prefix on suffix).
        assert_eq!(parse_chat_tmux_session("wg-workgraph", "workgraph"), None);
    }

    #[test]
    fn detects_legacy_coordinator_id() {
        assert!(is_legacy_coordinator_id(".coordinator-0"));
        assert!(is_legacy_coordinator_id(".coordinator-3"));
        assert!(is_legacy_coordinator_id(".coordinator-99"));
        assert!(is_legacy_coordinator_id(".coordinator"));
        assert!(!is_legacy_coordinator_id(".chat-0"));
        assert!(!is_legacy_coordinator_id(".chat-3"));
        assert!(!is_legacy_coordinator_id("coordinator-loop"));
        assert!(!is_legacy_coordinator_id("regular-task"));
    }
}
