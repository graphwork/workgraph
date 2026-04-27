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
pub const TOOL_RESULT: Color = Color::Indexed(252); // light gray

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
