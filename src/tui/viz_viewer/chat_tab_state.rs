//! Per-coordinator chat tab state inference.
//!
//! Each chat tab in the coordinator bar should reflect the state of that
//! particular chat — not just whether the global service daemon is alive.
//! This module computes that per-tab state by combining:
//!   - The service-daemon liveness flag (false → all tabs gray)
//!   - The presence of streaming partial response text on disk
//!   - For the active tab, the in-memory `ChatState` (pending requests,
//!     errors)
//!
//! Color semantics (per task tui-chat-tab):
//!   - Blue   → `Idle`           — supervisor alive, ready for input
//!   - Yellow → `Responding`     — LLM actively generating
//!   - Gray   → `SupervisorDown` — service stopped or supervisor died
//!   - Red    → `Error`          — chat in unrecoverable error state

use std::path::Path;

use ratatui::style::Color;

/// Visible state of a chat tab in the coordinator bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatTabState {
    /// Service stopped or supervisor died — chat cannot make progress.
    SupervisorDown,
    /// Supervisor alive, no in-flight LLM call — ready for user input.
    Idle,
    /// LLM is actively generating a response right now.
    Responding,
    /// Chat in unrecoverable error state.
    Error,
}

impl ChatTabState {
    /// Color used to render the tab's dot/label.
    pub fn color(self) -> Color {
        match self {
            Self::SupervisorDown => Color::DarkGray,
            Self::Idle => Color::Blue,
            Self::Responding => Color::Yellow,
            Self::Error => Color::Red,
        }
    }
}

/// Snapshot of the active-tab chat state used by [`infer`].
///
/// Decoupled from the full `ChatState` struct so tests can construct it
/// without spinning up an entire `VizApp`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ActiveChatSnapshot {
    /// True iff `pending_request_ids` is non-empty.
    pub awaiting_response: bool,
    /// True iff the chat is in an unrecoverable error state.
    pub error: bool,
}

/// Compute the visible state for a single chat tab.
///
/// Inputs:
///   - `workgraph_dir`: project root, used to read `.streaming` for inactive
///     tabs.
///   - `cid`: the coordinator id this tab represents.
///   - `service_alive`: whether the service daemon is currently running.
///   - `active_snapshot`: in-memory state for this coordinator if it is
///     currently the active tab; `None` for inactive tabs.
///
/// Precedence (highest first):
///   1. service down → `SupervisorDown`
///   2. active-tab error → `Error`
///   3. active-tab awaiting_response OR streaming file non-empty →
///      `Responding`
///   4. otherwise → `Idle`
pub fn infer(
    workgraph_dir: &Path,
    cid: u32,
    service_alive: bool,
    active_snapshot: Option<ActiveChatSnapshot>,
) -> ChatTabState {
    if !service_alive {
        return ChatTabState::SupervisorDown;
    }
    if let Some(snap) = active_snapshot {
        if snap.error {
            return ChatTabState::Error;
        }
        if snap.awaiting_response {
            return ChatTabState::Responding;
        }
        // Even on the active tab, fall through to disk check — there are
        // tiny windows during reconnect where pending_request_ids is empty
        // but the supervisor is still streaming. The disk file is
        // authoritative for "tokens currently being written."
    }
    if !workgraph::chat::read_streaming(workgraph_dir, cid).is_empty() {
        return ChatTabState::Responding;
    }
    ChatTabState::Idle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn supervisor_down_overrides_everything() {
        let tmp = tmp_dir();
        let snap = ActiveChatSnapshot {
            awaiting_response: true,
            error: true,
        };
        let state = infer(tmp.path(), 0, false, Some(snap));
        assert_eq!(state, ChatTabState::SupervisorDown);
        assert_eq!(state.color(), Color::DarkGray);
    }

    #[test]
    fn idle_when_service_alive_and_no_streaming() {
        let tmp = tmp_dir();
        let state = infer(tmp.path(), 0, true, None);
        assert_eq!(state, ChatTabState::Idle);
        assert_eq!(state.color(), Color::Blue);
    }

    #[test]
    fn idle_when_active_snapshot_is_quiet() {
        let tmp = tmp_dir();
        let snap = ActiveChatSnapshot {
            awaiting_response: false,
            error: false,
        };
        let state = infer(tmp.path(), 0, true, Some(snap));
        assert_eq!(state, ChatTabState::Idle);
        assert_eq!(state.color(), Color::Blue);
    }

    #[test]
    fn responding_when_active_snapshot_awaits() {
        let tmp = tmp_dir();
        let snap = ActiveChatSnapshot {
            awaiting_response: true,
            error: false,
        };
        let state = infer(tmp.path(), 0, true, Some(snap));
        assert_eq!(state, ChatTabState::Responding);
        assert_eq!(state.color(), Color::Yellow);
    }

    #[test]
    fn responding_when_streaming_file_nonempty() {
        let tmp = tmp_dir();
        // Write a streaming partial response to disk.
        workgraph::chat::write_streaming(tmp.path(), 3, "partial reply tokens").unwrap();
        let state = infer(tmp.path(), 3, true, None);
        assert_eq!(state, ChatTabState::Responding);
        assert_eq!(state.color(), Color::Yellow);
    }

    #[test]
    fn streaming_file_only_affects_matching_cid() {
        let tmp = tmp_dir();
        workgraph::chat::write_streaming(tmp.path(), 5, "tokens for 5").unwrap();
        // cid 7 has no streaming file — should be Idle.
        let state_other = infer(tmp.path(), 7, true, None);
        assert_eq!(state_other, ChatTabState::Idle);
        // cid 5 reflects the streaming file.
        let state_match = infer(tmp.path(), 5, true, None);
        assert_eq!(state_match, ChatTabState::Responding);
    }

    #[test]
    fn error_takes_precedence_over_responding() {
        let tmp = tmp_dir();
        let snap = ActiveChatSnapshot {
            awaiting_response: true,
            error: true,
        };
        let state = infer(tmp.path(), 0, true, Some(snap));
        assert_eq!(state, ChatTabState::Error);
        assert_eq!(state.color(), Color::Red);
    }
}
