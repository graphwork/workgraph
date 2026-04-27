//! Per-mode renderers for the per-task Log pane (right panel tab 4).
//!
//! Four view modes are supported (cycled with the `4` key):
//!
//! 1. **Events** — one structured line per event (tool calls, results,
//!    errors). Quick operational view.
//! 2. **HighLevel** — collapses adjacent same-kind activity into a
//!    coarse summary ("Editing src/cli.rs", "Running cargo test",
//!    "Reading config.toml"). Useful for monitoring multiple agents.
//! 3. **RawPretty** — full pretty-printed transcript: every event
//!    rendered with its own formatter, NEVER as a JSON dump. Each
//!    event-kind has a distinct prefix and visual treatment.
//! 4. **WgLog** — workgraph-level log entries only: `wg log` writes,
//!    dispatcher status updates, and task lifecycle transitions sourced
//!    from the task's `log` field on the graph. Contains no LLM-stream
//!    content (no tool calls, tokens, thinking, etc.) — useful for
//!    seeing only the structured workgraph-side narrative.
//!
//! The first three modes consume the same `&[AgentStreamEvent]` produced
//! by `parse_raw_stream_line`; WgLog consumes the pre-rendered
//! `[<rel-time>] <message>` strings populated by `load_log_pane()` from
//! `task.log`. Adding a new mode means adding one more function here —
//! no extra parsing or storage.
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
/// Tool calls render as a single inline line of the form
/// `{status?}⌁<Tool> → <detail>` where the optional status prefix is
/// `✓` (success) or `✗` (failure) once the tool's result has arrived,
/// or absent while the call is still in flight. The paired
/// `ToolResult` / `Error` event is consumed by the call's line and is
/// NOT emitted as a separate `✓ <result>` row that would visually
/// fracture the call from its outcome.
///
/// Non-tool events keep their parsed `summary`, one line per event.
pub fn render_events_view(events: &[AgentStreamEvent]) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut i = 0;
    while i < events.len() {
        let event = &events[i];

        if event.kind == AgentStreamEventKind::ToolCall {
            // Two ways a tool call can know its outcome:
            //   1. claude flow — a separate ToolResult/Error event is
            //      emitted immediately after this ToolCall.
            //   2. native-executor flow — the result preview is folded
            //      into the same event's summary as a "  ✓ …" /
            //      "  ✗ …" continuation line.
            let paired_status = events
                .get(i + 1)
                .and_then(|n| n.details.as_ref())
                .and_then(|d| match d {
                    EventDetails::ToolResult { is_error, .. } => Some(*is_error),
                    _ => None,
                });
            let status = paired_status.or_else(|| embedded_result_status(&event.summary));

            emit_tool_call_line(&mut out, event, status);

            if paired_status.is_some() {
                // Skip the now-folded result event so it doesn't render
                // as its own row.
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        let color = event_color(&event.kind);
        let modifier = event_modifier(&event.kind);
        for sub_line in event.summary.split('\n') {
            out.push(Line::from(Span::styled(
                sub_line.to_string(),
                Style::default().fg(color).add_modifier(modifier),
            )));
        }
        i += 1;
    }
    out
}

/// Detect a folded result-preview marker on a ToolCall summary.
/// Native-executor `tool_call` events embed "  ✓ <preview>" or
/// "  ✗ <preview>" as a continuation line; we promote that marker to
/// the call line itself and drop the bare preview row.
fn embedded_result_status(summary: &str) -> Option<bool> {
    for line in summary.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("✓ ") {
            return Some(false);
        }
        if trimmed.starts_with("✗ ") {
            return Some(true);
        }
    }
    None
}

/// Render one tool call as a single inline line (plus any non-result
/// continuation lines from the original summary). The optional `status`
/// becomes the leading prefix character (`✓` or `✗`); when `None` the
/// line shows just `⌁<Tool> → ...` to signal "in flight".
fn emit_tool_call_line(
    out: &mut Vec<Line<'static>>,
    event: &AgentStreamEvent,
    status: Option<bool>,
) {
    let tool_color = event_color(&event.kind);
    let mut emitted_call_line = false;

    for line in event.summary.split('\n') {
        let trimmed = line.trim_start();
        // Drop the bare "  ✓ <preview>" / "  ✗ <preview>" continuation
        // line emitted by the native-executor parser — its information
        // is now carried by the leading status prefix on the call line.
        if trimmed.starts_with("✓ ") || trimmed.starts_with("✗ ") {
            continue;
        }

        let mut spans: Vec<Span<'static>> = Vec::new();

        if !emitted_call_line {
            if let Some(is_error) = status {
                let (sym, color) = if is_error {
                    ("✗", Color::Red)
                } else {
                    ("✓", Color::Green)
                };
                spans.push(Span::styled(
                    sym.to_string(),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
            }
        }

        // Tighten the legacy "⌁ Tool → ..." form (with a space) to the
        // requested "⌁Tool → ..." (no space) so the lightning glyph
        // visually attaches to the tool name.
        let body = line.strip_prefix("⌁ ").map(|rest| format!("⌁{}", rest));
        let body_str = body.as_deref().unwrap_or(line);

        spans.push(Span::styled(
            body_str.to_string(),
            Style::default().fg(tool_color),
        ));
        out.push(Line::from(spans));
        emitted_call_line = true;
    }

    // Defensive: if the summary was empty (shouldn't happen for a
    // ToolCall), still emit a placeholder line so the prefix is visible.
    if !emitted_call_line {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if let Some(is_error) = status {
            let (sym, color) = if is_error {
                ("✗", Color::Red)
            } else {
                ("✓", Color::Green)
            };
            spans.push(Span::styled(
                sym.to_string(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::styled(
            "⌁".to_string(),
            Style::default().fg(tool_color),
        ));
        out.push(Line::from(spans));
    }
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

/// Coarse semantic grouping used to decide where blank-line gaps go in
/// the RawPretty view. Same-category neighbors run together; different
/// categories are separated by one blank line. See `render_raw_pretty_view`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventCategory {
    /// Tool calls + their results + tool-side errors. The agent is acting.
    Tool,
    /// User prompts, assistant text, internal thinking. The agent (or human) is speaking.
    Text,
    /// System / metadata events.
    System,
}

fn categorize(kind: &AgentStreamEventKind) -> EventCategory {
    match kind {
        AgentStreamEventKind::ToolCall
        | AgentStreamEventKind::ToolResult
        | AgentStreamEventKind::Error => EventCategory::Tool,
        AgentStreamEventKind::UserInput
        | AgentStreamEventKind::TextOutput
        | AgentStreamEventKind::Thinking => EventCategory::Text,
        AgentStreamEventKind::SystemEvent => EventCategory::System,
    }
}

/// Render the RawPretty view: full pretty-printed transcript of every
/// event. Crucially: NO raw JSON dumps — each event kind gets its own
/// formatter so the output reads as a clean transcript.
///
/// Blank-line policy: a blank line is inserted ONLY at category
/// boundaries (text↔tool, tool↔system, etc.), never between events of
/// the same category. This emphasizes the transition between speaking
/// and acting without adding noise to consecutive same-mode events.
pub fn render_raw_pretty_view(events: &[AgentStreamEvent]) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut prev_category: Option<EventCategory> = None;

    for event in events {
        let curr = categorize(&event.kind);
        if let Some(prev) = prev_category
            && prev != curr
        {
            push_blank(&mut out);
        }
        emit_event(&mut out, event);
        prev_category = Some(curr);
    }

    out
}

/// Render the WgLog view: workgraph-level entries only, sourced from
/// `task.log` via `load_log_pane()`. The caller passes the pre-formatted
/// `[<rel-time>] <message>` strings; this renderer styles them and
/// inserts a placeholder when there are no entries yet. NO LLM-stream
/// content appears here — that is intentional, this view is the "what
/// has the graph itself recorded for this task" surface.
pub fn render_wg_log_view(rendered_lines: &[String]) -> Vec<Line<'static>> {
    if rendered_lines.is_empty() {
        return vec![Line::from(Span::styled(
            "(no workgraph log entries yet)",
            Style::default().fg(Color::DarkGray),
        ))];
    }
    rendered_lines
        .iter()
        .map(|s| {
            Line::from(Span::styled(
                s.clone(),
                Style::default().fg(Color::Gray),
            ))
        })
        .collect()
}

fn emit_event(out: &mut Vec<Line<'static>>, event: &AgentStreamEvent) {
    let details = match &event.details {
        Some(d) => d,
        None => {
            // No structured details — fall back to the summary so we
            // never produce a totally empty section.
            push_header(out, &event.kind, "untyped");
            push_indented(out, &event.summary, Color::Gray);
            return;
        }
    };

    match details {
        EventDetails::UserInput { text } => {
            push_header(out, &event.kind, "[user]");
            push_indented(out, text, Color::Yellow);
        }
        EventDetails::TextOutput { text } => {
            push_header(out, &event.kind, "[assistant]");
            push_indented(out, text, Color::White);
        }
        EventDetails::Thinking { text } => {
            push_header(out, &event.kind, "<thinking>");
            push_indented(out, text, Color::Magenta);
            out.push(Line::from(Span::styled(
                "</thinking>".to_string(),
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::DIM),
            )));
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
                push_indented(out, &body, Color::Cyan);
            }
        }
        EventDetails::ToolResult { content, is_error } => {
            let marker = if *is_error { "✗" } else { "✓" };
            let color = if *is_error { Color::Red } else { Color::Green };
            let body: &str = if content.is_empty() {
                "(empty result)"
            } else {
                content.as_str()
            };
            push_marker_block(out, marker, body, color);
        }
        EventDetails::SystemEvent { subtype, text } => {
            out.push(Line::from(Span::styled(
                format!("⚙ system [{}]", subtype),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )));
            push_indented(out, text, Color::DarkGray);
        }
    }
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

/// Push a multi-line block with a single-character marker on the first
/// line and a 2-space hanging indent on continuation lines:
///
/// ```text
/// ✓ first content line
///   second content line
///   third content line
/// ```
///
/// The marker is bolded; the content is plain. With a single-char marker
/// this aligns content text at column 3 on every line.
fn push_marker_block(out: &mut Vec<Line<'static>>, marker: &str, body: &str, color: Color) {
    let mut iter = body.split('\n');
    let first = iter.next().unwrap_or("");
    out.push(Line::from(vec![
        Span::styled(
            format!("{} ", marker),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(first.to_string(), Style::default().fg(color)),
    ]));
    for src_line in iter {
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
            summary: format!("⌁ Bash → {}", cmd),
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
            summary: format!("⌁ Edit → {}", path),
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

    fn text_output(text: &str) -> AgentStreamEvent {
        AgentStreamEvent {
            kind: AgentStreamEventKind::TextOutput,
            agent_id: "agent-test".to_string(),
            summary: format!("[assistant] {}", text),
            details: Some(EventDetails::TextOutput {
                text: text.to_string(),
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

    fn line_text(line: &Line) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
    }

    fn is_blank(line: &Line) -> bool {
        line.spans.iter().all(|s| s.content.is_empty())
    }

    /// Hanging-indent rule for tool results: marker only on the first
    /// content line, 2-space pure padding on continuation lines, with no
    /// bare "result" header line.
    #[test]
    fn test_raw_mode_tool_result_uses_hanging_indent() {
        let body = "1 #!/usr/bin/env bash\n\
                    2 # Helpers shared by smoke-gate scenarios.\n\
                    3 #\n\
                    4 set -euo pipefail\n\
                    5 echo done";
        let events = vec![tool_result(body, false)];
        let lines = render_raw_pretty_view(&events);

        assert_eq!(
            lines.len(),
            5,
            "expected 5 rendered lines for 5-line content (no separate header), got {}: {}",
            lines.len(),
            lines_to_text(&lines)
        );

        let l0 = line_text(&lines[0]);
        assert_eq!(
            l0, "✓ 1 #!/usr/bin/env bash",
            "first line should merge marker with first content line, got: {:?}",
            l0
        );

        for (i, line) in lines.iter().enumerate().skip(1) {
            let t = line_text(line);
            assert!(
                t.starts_with("  "),
                "line {} must start with exactly 2 leading spaces (hanging indent), got {:?}",
                i,
                t
            );
            assert!(
                !t.starts_with("   "),
                "line {} must NOT have 3+ leading spaces, got {:?}",
                i,
                t
            );
        }

        let text = lines_to_text(&lines);
        assert!(
            !text.lines().any(|l| l == "✓ result" || l == "✗ result"),
            "bare 'result' header line must be removed: {}",
            text
        );
    }

    /// Hanging indent must also apply to error tool results.
    #[test]
    fn test_raw_mode_error_tool_result_uses_hanging_indent() {
        let body = "line one\nline two";
        let events = vec![tool_result(body, true)];
        let lines = render_raw_pretty_view(&events);

        assert_eq!(lines.len(), 2, "two content lines, no separate header");
        assert_eq!(line_text(&lines[0]), "✗ line one");
        assert_eq!(line_text(&lines[1]), "  line two");
    }

    /// Blank lines appear ONLY at category boundaries: text→tool and
    /// tool→text. No blank between a tool call and its result.
    #[test]
    fn test_raw_mode_inserts_blank_at_text_to_tool_boundary() {
        let events = vec![
            text_output("here is the plan"),
            tool_call_bash("cargo test"),
            tool_result("ok", false),
            text_output("done!"),
        ];
        let lines = render_raw_pretty_view(&events);
        let text = lines_to_text(&lines);

        let blanks: usize = lines.iter().filter(|l| is_blank(l)).count();
        assert_eq!(
            blanks, 2,
            "expected exactly 2 blank lines (text→tool and tool→text), got {} in:\n{}",
            blanks, text
        );

        // Verify there is NO blank line between the bash call and its result.
        // Find the line containing "Bash" and the line containing "✓ ok".
        let bash_idx = lines
            .iter()
            .position(|l| line_text(l).contains("Bash"))
            .expect("Bash call line missing");
        let ok_idx = lines
            .iter()
            .position(|l| line_text(l).starts_with("✓ ok"))
            .expect("'✓ ok' result line missing");
        assert!(bash_idx < ok_idx, "result must follow call");
        for line in &lines[bash_idx + 1..ok_idx] {
            assert!(
                !is_blank(line),
                "no blank line allowed between tool call and its result, got blank at:\n{}",
                text
            );
        }
    }

    /// Consecutive same-category events render with no blank lines
    /// between them — only one continuous text section.
    #[test]
    fn test_raw_mode_no_blank_between_consecutive_text_events() {
        let events = vec![
            text_output("part 1"),
            text_output("part 2"),
            text_output("part 3"),
        ];
        let lines = render_raw_pretty_view(&events);
        let text = lines_to_text(&lines);

        let blanks: usize = lines.iter().filter(|l| is_blank(l)).count();
        assert_eq!(
            blanks, 0,
            "consecutive same-category events must not have blank gaps, got {} blanks:\n{}",
            blanks, text
        );
    }

    /// A run of consecutive ToolCall events should render without blank
    /// lines between them either.
    #[test]
    fn test_raw_mode_no_blank_between_consecutive_tool_events() {
        let events = vec![
            tool_call_bash("cargo build"),
            tool_call_bash("cargo test"),
            tool_call_bash("cargo run"),
        ];
        let lines = render_raw_pretty_view(&events);
        let blanks: usize = lines.iter().filter(|l| is_blank(l)).count();
        assert_eq!(
            blanks,
            0,
            "consecutive tool events must not have blank gaps:\n{}",
            lines_to_text(&lines)
        );
    }

    /// WgLog mode renders one line per pre-formatted workgraph entry,
    /// preserving ordering. The renderer takes pre-formatted strings
    /// (`load_log_pane()` builds `[<rel-time>] <message>` lines from
    /// `task.log`); it must not drop or reorder them.
    #[test]
    fn test_wg_log_mode_renders_workgraph_entries_in_order() {
        let entries = vec![
            "[5m ago] Task created".to_string(),
            "[3m ago] Spawned by coordinator --executor claude --model opus".to_string(),
            "[1m ago] Starting implementation".to_string(),
        ];
        let lines = render_wg_log_view(&entries);
        let text = lines_to_text(&lines);

        assert_eq!(
            lines.len(),
            entries.len(),
            "one rendered line per entry, got: {}",
            text
        );
        for entry in &entries {
            assert!(
                text.contains(entry),
                "entry {:?} missing from rendered output:\n{}",
                entry,
                text
            );
        }
        // Order preserved.
        let pos_first = text.find("Task created").unwrap();
        let pos_mid = text.find("Spawned by coordinator").unwrap();
        let pos_last = text.find("Starting implementation").unwrap();
        assert!(
            pos_first < pos_mid && pos_mid < pos_last,
            "WgLog must preserve input order:\n{}",
            text
        );
    }

    /// WgLog mode shows a clear placeholder when the task has no
    /// workgraph-level log entries yet — the user should never see a
    /// silently empty pane.
    #[test]
    fn test_wg_log_mode_renders_placeholder_when_empty() {
        let lines = render_wg_log_view(&[]);
        let text = lines_to_text(&lines);
        assert_eq!(lines.len(), 1, "exactly one placeholder line when empty");
        assert!(
            text.contains("no workgraph log entries"),
            "placeholder text should signal emptiness, got: {}",
            text
        );
    }

    /// Events mode: a successful Bash call paired with its ToolResult
    /// must render as ONE inline line of the form `✓⌁Bash → <cmd>`,
    /// and the standalone result event must NOT appear as its own row
    /// (no `✓ Task: ...` line breaking the visual grouping).
    #[test]
    fn test_events_mode_paired_success_collapses_to_single_inline_line() {
        let events = vec![
            tool_call_bash("wg show test-task-2"),
            tool_result("Task: test-task-2\nStatus: open", false),
        ];
        let lines = render_events_view(&events);
        let text = lines_to_text(&lines);

        assert_eq!(
            lines.len(),
            1,
            "expected a single line (call + result folded together), got {}: {}",
            lines.len(),
            text
        );
        let l0 = line_text(&lines[0]);
        assert!(
            l0.starts_with("✓⌁Bash"),
            "expected line to start with '✓⌁Bash' (success prefix tight against ⌁), got: {:?}",
            l0
        );
        assert!(
            l0.contains("→ wg show test-task-2"),
            "expected arrow + command on same line, got: {:?}",
            l0
        );
        assert!(
            !text.contains("Task:"),
            "result body must NOT bleed into events mode — no '✓ Task: ...' row, got:\n{}",
            text
        );
        assert!(
            !text.contains("✓ Task:"),
            "explicit guard against the legacy '✓ Task: ...' standalone line, got:\n{}",
            text
        );
    }

    /// Events mode: a failed Bash call's status prefix must show on the
    /// SAME line as the call (no separate failure-result line).
    #[test]
    fn test_events_mode_paired_failure_uses_failure_prefix() {
        let events = vec![
            tool_call_bash("cargo test"),
            tool_result("compilation failed", true),
        ];
        let lines = render_events_view(&events);
        let text = lines_to_text(&lines);

        assert_eq!(
            lines.len(),
            1,
            "failure pair must also fold to one line: {}",
            text
        );
        let l0 = line_text(&lines[0]);
        assert!(
            l0.starts_with("✗⌁Bash"),
            "expected '✗⌁Bash' failure prefix, got: {:?}",
            l0
        );
        assert!(
            !text.contains("compilation failed"),
            "result body must not appear as its own row in events mode: {}",
            text
        );
    }

    /// Events mode: an in-flight tool call (no paired result yet)
    /// renders WITHOUT a leading status — just `⌁Bash → ...` — so the
    /// reader can tell the call hasn't finished.
    #[test]
    fn test_events_mode_inflight_call_has_no_status_prefix() {
        let events = vec![tool_call_bash("cargo build")];
        let lines = render_events_view(&events);
        let text = lines_to_text(&lines);

        assert_eq!(lines.len(), 1, "in-flight call is a single line: {}", text);
        let l0 = line_text(&lines[0]);
        assert!(
            l0.starts_with("⌁Bash"),
            "in-flight call must start with '⌁Bash' (no status prefix), got: {:?}",
            l0
        );
        assert!(
            !l0.starts_with("✓") && !l0.starts_with("✗"),
            "no leading status while pending, got: {:?}",
            l0
        );
    }

    /// Events mode: status-prefix logic applies uniformly across tool
    /// kinds (not just Bash). Smoke a Read and an Edit so a future
    /// refactor that special-cases Bash gets caught.
    #[test]
    fn test_events_mode_prefix_logic_applies_to_non_bash_tools() {
        let events = vec![
            tool_call_edit("src/main.rs", "old", "new"),
            tool_result("File edited", false),
        ];
        let lines = render_events_view(&events);
        let l0 = line_text(&lines[0]);
        assert!(
            l0.starts_with("✓⌁Edit"),
            "Edit success must use '✓⌁Edit' prefix, got: {:?}",
            l0
        );
        assert!(
            l0.contains("src/main.rs"),
            "Edit line must include the file path, got: {:?}",
            l0
        );

        let read_call = AgentStreamEvent {
            kind: AgentStreamEventKind::ToolCall,
            agent_id: "agent-test".to_string(),
            summary: "⌁ Read → src/lib.rs".to_string(),
            details: Some(EventDetails::ToolCall {
                name: "Read".to_string(),
                input: serde_json::json!({"file_path": "src/lib.rs"}),
            }),
        };
        let read_evts = vec![read_call, tool_result("contents", false)];
        let lines2 = render_events_view(&read_evts);
        let l1 = line_text(&lines2[0]);
        assert!(
            l1.starts_with("✓⌁Read"),
            "Read success must use '✓⌁Read' prefix, got: {:?}",
            l1
        );
    }

    /// Events mode: native-executor `tool_call` events carry their own
    /// folded result preview as a `\n  ✓ ...` continuation line. The
    /// events renderer must promote that marker to the call-line prefix
    /// and drop the bare preview row, just like the claude-flow case.
    #[test]
    fn test_events_mode_folds_native_executor_embedded_result() {
        let event = AgentStreamEvent {
            kind: AgentStreamEventKind::ToolCall,
            agent_id: "agent-test".to_string(),
            summary: "⌁ Bash → ls -la\n  ✓ total 8".to_string(),
            details: Some(EventDetails::ToolCall {
                name: "Bash".to_string(),
                input: serde_json::json!({"command": "ls -la"}),
            }),
        };
        let lines = render_events_view(&[event]);
        assert_eq!(
            lines.len(),
            1,
            "native event must collapse to one line: {}",
            lines_to_text(&lines)
        );
        let l0 = line_text(&lines[0]);
        assert!(
            l0.starts_with("✓⌁Bash"),
            "embedded result marker must promote to a leading prefix, got: {:?}",
            l0
        );
        assert!(
            !l0.contains("total 8"),
            "result preview body should not bleed onto the call line: {:?}",
            l0
        );
    }

    /// Events mode does NOT alter the high-level or raw-pretty modes —
    /// they share the same input slice but route through their own
    /// renderers. Snapshot the line counts to catch accidental coupling.
    #[test]
    fn test_events_mode_does_not_affect_other_view_modes() {
        let events = vec![
            tool_call_bash("cargo test"),
            tool_result("ok", false),
            tool_call_edit("src/main.rs", "a", "b"),
            tool_result("edit applied", false),
        ];

        let high = render_high_level_view(&events);
        let high_text = lines_to_text(&high);
        // High-level still summarizes calls coarsely and hides results.
        assert!(
            high_text.contains("Running cargo"),
            "high-level renderer untouched, got: {}",
            high_text
        );
        assert!(
            high_text.contains("Editing"),
            "high-level renderer untouched, got: {}",
            high_text
        );

        let raw = render_raw_pretty_view(&events);
        let raw_text = lines_to_text(&raw);
        // Raw still uses the tighter "⌁ <label>" header per call AND
        // emits the result with its own ✓ marker block.
        assert!(
            raw_text.contains("⌁ Bash → \"cargo test\""),
            "raw renderer untouched, got: {}",
            raw_text
        );
        assert!(
            raw_text.contains("✓ ok"),
            "raw renderer must still emit result rows, got: {}",
            raw_text
        );
    }

    /// WgLog mode is the "structured workgraph narrative" view: it must
    /// NOT include any LLM-stream content. The renderer takes only
    /// `&[String]` (workgraph log entries from `task.log`), so by
    /// construction it cannot show stream events — but assert this
    /// holds at the API boundary so a future refactor doesn't quietly
    /// invert the contract.
    #[test]
    fn test_wg_log_mode_does_not_render_stream_events() {
        let stream_events = vec![
            tool_call_bash("cargo test"),
            text_output("here is a long assistant text response"),
            tool_call_edit("src/main.rs", "old text", "new text"),
        ];
        // Only LLM-stream events are present; the WgLog input slice is empty.
        let wg_entries: Vec<String> = Vec::new();
        let lines = render_wg_log_view(&wg_entries);
        let text = lines_to_text(&lines);

        // None of the LLM-stream content should leak into the WgLog view.
        for ev in &stream_events {
            assert!(
                !text.contains(&ev.summary),
                "WgLog must not contain LLM-stream content {:?}; got: {}",
                ev.summary,
                text
            );
        }
        assert!(
            !text.contains("cargo test"),
            "WgLog must not contain tool-call commands: {}",
            text
        );
        assert!(
            !text.contains("assistant text response"),
            "WgLog must not contain assistant text: {}",
            text
        );
    }
}
