//! Per-mode renderers for the per-task Log pane (right panel tab 4).
//!
//! Three view modes are supported (cycled with the `4` key):
//!
//! 1. **Events** — one structured line per event (tool calls, results,
//!    errors). Quick operational view.
//! 2. **HighLevel** — collapses adjacent same-kind activity into a
//!    coarse summary ("Editing src/cli.rs", "Running cargo test",
//!    "Reading config.toml"). Useful for monitoring multiple agents.
//! 3. **RawPretty** — full pretty-printed transcript: every event
//!    rendered with its own formatter, NEVER as a JSON dump. Each
//!    event-kind has a distinct prefix and visual treatment.
//!
//! All three modes consume the same `&[AgentStreamEvent]` produced by
//! `parse_raw_stream_line`, so adding a new mode means adding one more
//! function here — no extra parsing or storage.
//!
//! Pure functions — no `VizApp` dependency — so they unit-test cleanly.
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::chat_palette;
use super::state::{AgentStreamEvent, AgentStreamEventKind, EventDetails};

/// Convert an event kind to its display color, using the shared
/// `chat_palette` so structure/role coloring stays coherent across the
/// chat and Log surfaces.
fn event_color(kind: &AgentStreamEventKind) -> Color {
    match kind {
        AgentStreamEventKind::ToolCall => chat_palette::TOOL_CALL,
        AgentStreamEventKind::ToolResult => chat_palette::DEFAULT_TEXT,
        AgentStreamEventKind::TextOutput => chat_palette::DEFAULT_TEXT,
        AgentStreamEventKind::Thinking => chat_palette::THINKING,
        AgentStreamEventKind::SystemEvent => Color::DarkGray,
        AgentStreamEventKind::Error => chat_palette::ERROR,
        AgentStreamEventKind::UserInput => chat_palette::USER_PREFIX,
    }
}

/// Optional modifier per kind (e.g. italic for thinking).
fn event_modifier(kind: &AgentStreamEventKind) -> Modifier {
    match kind {
        AgentStreamEventKind::Thinking => Modifier::ITALIC,
        _ => Modifier::empty(),
    }
}

/// Render the Events view: one summary line per event.
///
/// This preserves the legacy "view=activity" behavior — one line per
/// stream event, tinted by kind.
pub fn render_events_view(events: &[AgentStreamEvent]) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    for event in events {
        let color = event_color(&event.kind);
        let modifier = event_modifier(&event.kind);
        for sub_line in event.summary.split('\n') {
            out.push(Line::from(Span::styled(
                sub_line.to_string(),
                Style::default().fg(color).add_modifier(modifier),
            )));
        }
    }
    out
}

/// Compute a "coarse activity" label for an event in HighLevel mode.
///
/// Returns `None` when the event should be hidden in this view (notably
/// tool results — implicit follow-ons of their tool call).
fn high_level_label(event: &AgentStreamEvent) -> Option<String> {
    match (&event.kind, event.details.as_ref()) {
        (AgentStreamEventKind::ToolCall, Some(EventDetails::ToolCall { name, input })) => {
            let target = match name.as_str() {
                "Bash" | "bash" => input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|c| {
                        let first = c.split_whitespace().next().unwrap_or("");
                        format!("Running {}", first)
                    }),
                "Read" => input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .map(|p| format!("Reading {}", basename(p))),
                "Write" => input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .map(|p| format!("Writing {}", basename(p))),
                "Edit" => input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .map(|p| format!("Editing {}", basename(p))),
                "Grep" => input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .map(|p| format!("Searching for `{}`", p)),
                "Glob" => input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .map(|p| format!("Finding files matching `{}`", p)),
                other => Some(format!("Using {}", other)),
            };
            Some(target.unwrap_or_else(|| format!("Using {}", name)))
        }
        // Hide tool results in the high-level view — the activity is the
        // tool call itself, the result is implicit follow-up.
        (AgentStreamEventKind::ToolResult, _) => None,
        // Errors are loud — keep them visible.
        (AgentStreamEventKind::Error, _) => Some("Tool errored".to_string()),
        (AgentStreamEventKind::Thinking, _) => Some("Thinking…".to_string()),
        (AgentStreamEventKind::TextOutput, _) => Some("Speaking".to_string()),
        (AgentStreamEventKind::UserInput, _) => Some("User prompt".to_string()),
        (AgentStreamEventKind::SystemEvent, _) => Some("System event".to_string()),
        // ToolCall without (or with mismatched) details — fall back to summary.
        (AgentStreamEventKind::ToolCall, _) => Some(event.summary.clone()),
    }
}

/// Render the HighLevel view: one line per coarse activity, with
/// adjacent identical activities collapsed into "Activity (xN)".
pub fn render_high_level_view(events: &[AgentStreamEvent]) -> Vec<Line<'static>> {
    let mut entries: Vec<(String, AgentStreamEventKind, usize)> = Vec::new();
    for event in events {
        let label = match high_level_label(event) {
            Some(l) => l,
            None => continue,
        };
        if let Some(last) = entries.last_mut()
            && last.0 == label
            && last.1 == event.kind
        {
            last.2 += 1;
            continue;
        }
        entries.push((label, event.kind.clone(), 1));
    }

    entries
        .into_iter()
        .map(|(label, kind, count)| {
            let display = if count > 1 {
                format!("• {} (x{})", label, count)
            } else {
                format!("• {}", label)
            };
            Line::from(Span::styled(
                display,
                Style::default().fg(event_color(&kind)),
            ))
        })
        .collect()
}

/// Render the RawPretty view: full pretty-printed transcript of every
/// event. Crucially: NO raw JSON dumps — each event kind gets its own
/// formatter so the output reads as a clean transcript.
pub fn render_raw_pretty_view(events: &[AgentStreamEvent]) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    for event in events {
        let details = match &event.details {
            Some(d) => d,
            None => {
                // No structured details — fall back to the summary so
                // we never produce a totally empty section.
                push_header(&mut out, &event.kind, "untyped");
                push_indented(&mut out, &event.summary, Color::Gray);
                push_blank(&mut out);
                continue;
            }
        };

        match details {
            EventDetails::UserInput { text } => {
                push_header(&mut out, &event.kind, "[user]");
                push_indented(&mut out, text, Color::Yellow);
                push_blank(&mut out);
            }
            EventDetails::TextOutput { text } => {
                push_header(&mut out, &event.kind, "[assistant]");
                push_indented(&mut out, text, Color::White);
                push_blank(&mut out);
            }
            EventDetails::Thinking { text } => {
                push_header(&mut out, &event.kind, "<thinking>");
                push_indented(&mut out, text, Color::Magenta);
                out.push(Line::from(Span::styled(
                    "</thinking>".to_string(),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::DIM),
                )));
                push_blank(&mut out);
            }
            EventDetails::ToolCall { name, input } => {
                let label = format_tool_call_label(name, input);
                out.push(Line::from(Span::styled(
                    format!("⌁ {}", label),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )));
                let body = format_tool_call_body(name, input);
                if !body.is_empty() {
                    push_indented(&mut out, &body, Color::Cyan);
                }
                push_blank(&mut out);
            }
            EventDetails::ToolResult { content, is_error } => {
                let prefix = if *is_error { "✗ result" } else { "✓ result" };
                let color = if *is_error { Color::Red } else { Color::Green };
                out.push(Line::from(Span::styled(
                    prefix.to_string(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                )));
                let snippet = if content.is_empty() {
                    "(empty result)".to_string()
                } else {
                    content.clone()
                };
                push_indented(&mut out, &snippet, color);
                push_blank(&mut out);
            }
            EventDetails::SystemEvent { subtype, text } => {
                out.push(Line::from(Span::styled(
                    format!("⚙ system [{}]", subtype),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )));
                push_indented(&mut out, text, Color::DarkGray);
                push_blank(&mut out);
            }
        }
    }

    // Trim trailing blank line for cleaner display.
    while matches!(out.last(), Some(line) if line.spans.is_empty()) {
        out.pop();
    }
    out
}

/// Emit the section header used by every event-kind in raw mode.
fn push_header(out: &mut Vec<Line<'static>>, kind: &AgentStreamEventKind, tag: &str) {
    let color = event_color(kind);
    out.push(Line::from(Span::styled(
        tag.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
}

/// Push a blank separator line.
fn push_blank(out: &mut Vec<Line<'static>>) {
    out.push(Line::from(""));
}

/// Push `body`, indented two spaces and styled with `color`.
/// Multiline input is split into one Line per source line.
fn push_indented(out: &mut Vec<Line<'static>>, body: &str, color: Color) {
    for src_line in body.split('\n') {
        out.push(Line::from(Span::styled(
            format!("  {}", src_line),
            Style::default().fg(color),
        )));
    }
}

/// Strip leading directory components from a path-like string.
/// Used by the high-level renderer so "Editing src/foo/bar.rs" becomes
/// "Editing bar.rs" when the path is long enough to feel noisy. We keep
/// up to two trailing path components for context.
fn basename(p: &str) -> String {
    let parts: Vec<&str> = p.rsplit(['/', '\\']).take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

/// Format the single-line label for a tool call in raw mode, e.g.
/// `Bash → "cargo test"` or `Edit → src/main.rs`.
fn format_tool_call_label(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Bash" | "bash" => {
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                let one_line = cmd.lines().next().unwrap_or(cmd);
                let one_line = if one_line.len() > 80 {
                    format!("{}…", &one_line[..one_line.floor_char_boundary(80)])
                } else {
                    one_line.to_string()
                };
                format!("Bash → \"{}\"", one_line)
            } else {
                "Bash".to_string()
            }
        }
        "Read" | "Write" | "Edit" => {
            if let Some(p) = input.get("file_path").and_then(|v| v.as_str()) {
                format!("{} → {}", name, p)
            } else {
                name.to_string()
            }
        }
        "Grep" | "Glob" => {
            if let Some(p) = input.get("pattern").and_then(|v| v.as_str()) {
                format!("{} → \"{}\"", name, p)
            } else {
                name.to_string()
            }
        }
        other => other.to_string(),
    }
}

/// Format the body of a tool call for raw mode. Returns a possibly-empty
/// string formatted as a transcript, NEVER as a JSON dump. For tools
/// where the call label already conveys everything (Bash one-liner,
/// Read), the body is empty and only the label is shown.
fn format_tool_call_body(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Bash" | "bash" => {
            // Body shown only when the command is multiline (we already
            // showed line one in the label).
            if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                let lines: Vec<&str> = cmd.lines().collect();
                if lines.len() > 1 {
                    // Skip the first line (already in label).
                    lines[1..].join("\n")
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        }
        "Edit" => {
            // Edit shows old → new diff snippet.
            let old = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if old.is_empty() && new.is_empty() {
                String::new()
            } else {
                let old_preview = preview_block(old);
                let new_preview = preview_block(new);
                format!("- {}\n+ {}", old_preview, new_preview)
            }
        }
        "Write" => {
            // Show first few lines of content if present.
            let content = input
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if content.is_empty() {
                String::new()
            } else {
                preview_block(content)
            }
        }
        _ => {
            // Unknown tool: render input fields (shallow), one per line.
            // NEVER as a single JSON blob.
            if let Some(obj) = input.as_object() {
                let mut buf = String::new();
                for (k, v) in obj.iter() {
                    let val_str = match v {
                        serde_json::Value::String(s) => preview_block(s),
                        other => other.to_string(),
                    };
                    if !buf.is_empty() {
                        buf.push('\n');
                    }
                    buf.push_str(&format!("{}: {}", k, val_str));
                }
                buf
            } else {
                String::new()
            }
        }
    }
}

/// Truncate a multi-line string to a few lines, replacing the rest with
/// an ellipsis marker. Single-line strings are left alone (up to 200
/// chars, then truncated).
fn preview_block(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() > 6 {
        let head = lines[..6].join("\n");
        format!("{}\n…(+{} lines)", head, lines.len() - 6)
    } else if s.len() > 200 {
        format!("{}…", &s[..s.floor_char_boundary(200)])
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_event(text: &str) -> AgentStreamEvent {
        AgentStreamEvent {
            kind: AgentStreamEventKind::UserInput,
            agent_id: "agent-test".to_string(),
            summary: format!("👤 {}", text),
            details: Some(EventDetails::UserInput {
                text: text.to_string(),
            }),
        }
    }

    fn tool_call_bash(cmd: &str) -> AgentStreamEvent {
        let input = serde_json::json!({"command": cmd});
        AgentStreamEvent {
            kind: AgentStreamEventKind::ToolCall,
            agent_id: "agent-test".to_string(),
            summary: format!("⚡ Bash → {}", cmd),
            details: Some(EventDetails::ToolCall {
                name: "Bash".to_string(),
                input,
            }),
        }
    }

    fn tool_call_edit(path: &str, old: &str, new: &str) -> AgentStreamEvent {
        let input = serde_json::json!({
            "file_path": path,
            "old_string": old,
            "new_string": new,
        });
        AgentStreamEvent {
            kind: AgentStreamEventKind::ToolCall,
            agent_id: "agent-test".to_string(),
            summary: format!("⚡ Edit → {}", path),
            details: Some(EventDetails::ToolCall {
                name: "Edit".to_string(),
                input,
            }),
        }
    }

    fn tool_result(content: &str, is_error: bool) -> AgentStreamEvent {
        let prefix = if is_error { "✗" } else { "✓" };
        AgentStreamEvent {
            kind: if is_error {
                AgentStreamEventKind::Error
            } else {
                AgentStreamEventKind::ToolResult
            },
            agent_id: "agent-test".to_string(),
            summary: format!("{} {}", prefix, content),
            details: Some(EventDetails::ToolResult {
                content: content.to_string(),
                is_error,
            }),
        }
    }

    fn lines_to_text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// RAW mode must render user messages with the [user] header and
    /// the prompt body — pretty-printed, NOT as JSON.
    #[test]
    fn test_raw_mode_renders_user_messages_pretty() {
        let events = vec![user_event("please add a feature flag")];
        let lines = render_raw_pretty_view(&events);
        let text = lines_to_text(&lines);

        assert!(
            text.contains("[user]"),
            "raw mode should label user input: {}",
            text
        );
        assert!(
            text.contains("please add a feature flag"),
            "raw mode should include user prompt body: {}",
            text
        );
        // Crucially: no JSON noise.
        assert!(
            !text.contains("\"type\""),
            "raw mode must NOT show raw JSON: {}",
            text
        );
        assert!(
            !text.contains("{\"message\""),
            "raw mode must NOT dump JSON objects: {}",
            text
        );
    }

    /// RAW mode must render tool calls with their tool name, parameters
    /// formatted as a transcript — never as a JSON blob.
    #[test]
    fn test_raw_mode_renders_tool_calls_pretty_not_json() {
        let events = vec![
            tool_call_bash("cargo test"),
            tool_call_edit("src/main.rs", "old text", "new text"),
        ];
        let lines = render_raw_pretty_view(&events);
        let text = lines_to_text(&lines);

        // Bash call rendered as transcript.
        assert!(
            text.contains("Bash"),
            "raw mode should name the tool: {}",
            text
        );
        assert!(
            text.contains("cargo test"),
            "raw mode should show the command: {}",
            text
        );

        // Edit call rendered as transcript with old/new diff lines.
        assert!(
            text.contains("Edit"),
            "raw mode should name the Edit tool: {}",
            text
        );
        assert!(
            text.contains("src/main.rs"),
            "raw mode should show the file_path: {}",
            text
        );
        assert!(
            text.contains("old text"),
            "raw mode should show the old_string in diff form: {}",
            text
        );
        assert!(
            text.contains("new text"),
            "raw mode should show the new_string in diff form: {}",
            text
        );

        // No JSON.
        assert!(
            !text.contains("\"command\":"),
            "raw mode must NOT emit JSON keys: {}",
            text
        );
        assert!(
            !text.contains("\"file_path\":"),
            "raw mode must NOT emit JSON keys: {}",
            text
        );
        assert!(
            !text.contains("\"old_string\":"),
            "raw mode must NOT emit JSON keys: {}",
            text
        );
    }

    /// HighLevel mode must collapse a noisy event stream into a much
    /// shorter sequence of coarse activity entries — and must hide
    /// tool results (which are implicit follow-ons of their calls).
    #[test]
    fn test_high_level_mode_summarizes_events() {
        let events = vec![
            tool_call_bash("cargo build"),
            tool_result("Compiling...", false),
            tool_call_bash("cargo test"),
            tool_result("test_foo passes", false),
            tool_call_edit("src/cli.rs", "a", "b"),
            tool_result("edit applied", false),
            tool_call_edit("src/cli.rs", "c", "d"),
            tool_result("edit applied", false),
        ];

        let high = render_high_level_view(&events);
        let events_view = render_events_view(&events);

        // High-level must be strictly shorter than the events view (it
        // is a summarization).
        assert!(
            high.len() < events_view.len(),
            "high-level view should be shorter than events view: high={} events={}",
            high.len(),
            events_view.len()
        );

        let high_text = lines_to_text(&high);
        // It should mention the activities, named meaningfully:
        assert!(
            high_text.contains("Running cargo")
                || high_text.contains("Running cargo build")
                || high_text.contains("Running cargo test"),
            "high-level should describe Bash calls coarsely: {}",
            high_text
        );
        assert!(
            high_text.contains("Editing")
                && (high_text.contains("cli.rs") || high_text.contains("src/cli.rs")),
            "high-level should describe Edits coarsely with file: {}",
            high_text
        );

        // Tool results are implicit and must NOT show as their own line.
        assert!(
            !high_text.contains("test_foo passes"),
            "high-level must hide tool result content: {}",
            high_text
        );
        assert!(
            !high_text.contains("edit applied"),
            "high-level must hide tool result content: {}",
            high_text
        );

        // Adjacent identical edits must collapse with a count marker.
        assert!(
            high_text.contains("(x2)"),
            "high-level should collapse adjacent identical activities: {}",
            high_text
        );
    }
}
