//! Semantic color palette for the chat / Log views.
//!
//! Principle: **default terminal text reads as default — color is for
//! structure/role, not for showing 'this is text'**. Coloring every span
//! drowns out the meaning that color is supposed to carry.
//!
//! - Bulk content (assistant prose, tool output bodies, attachment metadata)
//!   uses `Color::Reset` or a dim gray — the terminal's default foreground.
//! - Color is reserved for role prefixes, structural borders, and status
//!   accents (errors, warnings, badges).
//!
//! Future per-theme overrides should hang off this module so that touching
//! any chat color is a one-edit change.
//!
//! See task `tui-chat-smart` for the originating user feedback.
#![allow(dead_code)]

use ratatui::style::{Color, Modifier, Style};

/// Default text — explicit `Color::Reset` so spans do not inherit a leftover
/// foreground from a surrounding styled span.
pub const DEFAULT_TEXT: Color = Color::Reset;

/// Subtle metadata (attachment names, timestamps, "(edited)", read receipts).
/// Visible but not eye-catching — clearly metadata, not content.
pub const METADATA: Color = Color::DarkGray;

/// Tool-box border characters (┌─, │, └─). Dim so the box reads as structure
/// rather than competing with the content inside.
pub const TOOL_BORDER: Color = Color::DarkGray;

/// Tool-call name in a tool-box header. Semantically: "this is a structured
/// invocation, not prose".
pub const TOOL_CALL: Color = Color::Indexed(75); // soft cyan

/// Tool result body — readable but distinguishable from assistant prose.
/// On dark terminals: light gray (Indexed 252). On light terminals: terminal default.
pub const TOOL_RESULT: Color = Color::Indexed(252); // light gray, dark-theme only

/// Theme-aware tool result body color.
pub fn tool_result_color(is_light: bool) -> Color {
    if is_light {
        Color::Reset
    } else {
        TOOL_RESULT
    }
}

/// Tool error / failure surface — jumps out so failures don't get lost in
/// a wash of normal output.
pub const TOOL_ERROR: Color = Color::Red;

/// Errors surfaced inline in chat (system errors, agent crashes).
pub const ERROR: Color = Color::Red;

/// Warnings / cautionary notes.
pub const WARN: Color = Color::Yellow;

/// Informational system notes — distinct from regular content but not
/// alarming. Yellow rather than green so it doesn't blend into status badges.
pub const INFO: Color = Color::Yellow;

/// Thinking / chain-of-thought blocks. De-emphasized — italic + dim.
pub const THINKING: Color = Color::DarkGray;

/// User role prefix (e.g. "erik: ").
pub const USER_PREFIX: Color = Color::Yellow;

/// Coordinator / assistant role prefix (e.g. "↯ ").
pub const COORDINATOR_PREFIX: Color = Color::Cyan;

/// SentMessage role prefix ("→ task: ").
pub const SENT_MESSAGE_PREFIX: Color = Color::Magenta;

/// Status indicator: in-progress / active / healthy.
pub const STATUS_OK: Color = Color::Green;

/// Status indicator: caution / pending / awaiting.
pub const STATUS_WARN: Color = Color::Yellow;

/// Status indicator: down / failed.
pub const STATUS_DOWN: Color = Color::Red;

/// Style for thinking blocks — italic + dim foreground.
pub fn thinking_style() -> Style {
    Style::default().fg(THINKING).add_modifier(Modifier::ITALIC)
}

/// Style for attachment metadata lines (e.g. "[Attached: foo.png]").
/// Italic gray — readable, but clearly metadata.
pub fn attachment_style() -> Style {
    Style::default().fg(METADATA).add_modifier(Modifier::ITALIC)
}

/// Map a chat agent task ID to its label color for tab bars and node labels.
///
/// Single entry point for the `.chat-N` vs `.coordinator-N` visual split.
/// - `.chat-N` (current) → use `state_color` as-is (caller supplies blue/yellow/gray/red)
/// - `.coordinator-N` (legacy) → always muted gray, ignoring state color
pub fn chat_task_label_color(task_id: &str, state_color: Color) -> Color {
    if workgraph::chat_id::is_legacy_coordinator_id(task_id) {
        Color::Rgb(110, 110, 110)
    } else {
        state_color
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_task_gets_state_color() {
        let blue = Color::Blue;
        let result = chat_task_label_color(".chat-3", blue);
        assert_eq!(result, Color::Blue, ".chat-N should pass through the state color");
    }

    #[test]
    fn legacy_coordinator_gets_muted_color() {
        let blue = Color::Blue;
        let result = chat_task_label_color(".coordinator-3", blue);
        assert_ne!(
            result, Color::Blue,
            ".coordinator-N should NOT use the accent/state color"
        );
        assert_eq!(
            result,
            Color::Rgb(110, 110, 110),
            ".coordinator-N should be muted gray"
        );
    }

    #[test]
    fn legacy_bare_coordinator_is_muted() {
        let result = chat_task_label_color(".coordinator", Color::Yellow);
        assert_eq!(result, Color::Rgb(110, 110, 110));
    }

    #[test]
    fn chat_and_coordinator_styles_differ() {
        let state_color = Color::Blue;
        let chat_color = chat_task_label_color(".chat-1", state_color);
        let coord_color = chat_task_label_color(".coordinator-1", state_color);
        assert_ne!(
            chat_color, coord_color,
            ".chat-N and .coordinator-N must produce different colors"
        );
    }
}
