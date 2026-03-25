//! Event tracing for TUI sessions.
//!
//! Records all user interactions (key presses, mouse events, resize, paste,
//! focus changes) to a JSONL file for replay-based screencasts.
//!
//! Design goals:
//! - Zero overhead when tracing is disabled (no allocations, no syscalls).
//! - Minimal overhead when enabled (buffered writes, monotonic timestamps).
//! - One JSON object per line — trivially parseable.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyModifiers, MouseEventKind};
use serde::Serialize;

use super::state::{FocusedPanel, RightPanelTab};

/// Serializable snapshot of the TUI state at the moment an event fires.
#[derive(Serialize)]
pub struct StateContext {
    /// Which panel has keyboard focus.
    pub focused_panel: &'static str,
    /// Active right-panel tab (if right panel is visible).
    pub right_panel_tab: Option<&'static str>,
    /// Currently selected task ID (if any).
    pub selected_task: Option<String>,
    /// Whether the right panel is visible.
    pub right_panel_visible: bool,
    /// Whether search is active.
    pub search_active: bool,
    /// Whether help overlay is shown.
    pub show_help: bool,
}

/// A single trace entry written as one JSONL line.
#[derive(Serialize)]
pub struct TraceEntry {
    /// Monotonic timestamp in fractional seconds since trace start.
    pub t: f64,
    /// Event type discriminator.
    pub event: TracedEvent,
    /// TUI state snapshot at the time of the event.
    pub state: StateContext,
}

/// The event payload — a simplified, serializable representation of crossterm events.
#[derive(Serialize)]
#[serde(tag = "type")]
pub enum TracedEvent {
    Key {
        code: String,
        modifiers: String,
    },
    Mouse {
        kind: String,
        row: u16,
        col: u16,
    },
    Paste {
        len: usize,
    },
    Resize {
        width: u16,
        height: u16,
    },
    FocusGained,
    FocusLost,
}

/// Buffered writer for trace output. When `None`, tracing is disabled.
pub struct EventTracer {
    writer: BufWriter<File>,
    start: Instant,
}

impl EventTracer {
    /// Open a new trace file for writing. Returns an error if the file cannot be created.
    pub fn new(path: &Path) -> std::io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            writer: BufWriter::new(file),
            start: Instant::now(),
        })
    }

    /// Record a crossterm event with the current TUI state context.
    pub fn record(&mut self, ev: &Event, ctx: StateContext) {
        let t = self.start.elapsed().as_secs_f64();
        let event = match ev {
            Event::Key(key) => TracedEvent::Key {
                code: format_key_code(&key.code),
                modifiers: format_modifiers(key.modifiers),
            },
            Event::Mouse(mouse) => TracedEvent::Mouse {
                kind: format_mouse_kind(&mouse.kind),
                row: mouse.row,
                col: mouse.column,
            },
            Event::Paste(text) => TracedEvent::Paste { len: text.len() },
            Event::Resize(w, h) => TracedEvent::Resize {
                width: *w,
                height: *h,
            },
            Event::FocusGained => TracedEvent::FocusGained,
            Event::FocusLost => TracedEvent::FocusLost,
        };

        let entry = TraceEntry { t, event, state: ctx };
        // Best-effort: ignore write errors to avoid disrupting the TUI.
        let _ = serde_json::to_writer(&mut self.writer, &entry);
        let _ = self.writer.write_all(b"\n");
    }

    /// Flush any buffered data to disk.
    pub fn flush(&mut self) {
        let _ = self.writer.flush();
    }
}

impl Drop for EventTracer {
    fn drop(&mut self) {
        self.flush();
    }
}

// ── Formatting helpers ──────────────────────────────────────────────────────

fn format_key_code(code: &KeyCode) -> String {
    match code {
        KeyCode::Char(c) => format!("{c}"),
        KeyCode::F(n) => format!("F{n}"),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        KeyCode::PageUp => "PageUp".into(),
        KeyCode::PageDown => "PageDown".into(),
        KeyCode::Tab => "Tab".into(),
        KeyCode::BackTab => "BackTab".into(),
        KeyCode::Delete => "Delete".into(),
        KeyCode::Insert => "Insert".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::CapsLock => "CapsLock".into(),
        KeyCode::ScrollLock => "ScrollLock".into(),
        KeyCode::NumLock => "NumLock".into(),
        KeyCode::PrintScreen => "PrintScreen".into(),
        KeyCode::Pause => "Pause".into(),
        KeyCode::Menu => "Menu".into(),
        KeyCode::KeypadBegin => "KeypadBegin".into(),
        _ => format!("{code:?}"),
    }
}

fn format_modifiers(mods: KeyModifiers) -> String {
    let mut parts = Vec::new();
    if mods.contains(KeyModifiers::SHIFT) {
        parts.push("Shift");
    }
    if mods.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl");
    }
    if mods.contains(KeyModifiers::ALT) {
        parts.push("Alt");
    }
    if mods.contains(KeyModifiers::SUPER) {
        parts.push("Super");
    }
    if mods.contains(KeyModifiers::HYPER) {
        parts.push("Hyper");
    }
    if mods.contains(KeyModifiers::META) {
        parts.push("Meta");
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("+")
    }
}

fn format_mouse_kind(kind: &MouseEventKind) -> String {
    match kind {
        MouseEventKind::Down(btn) => format!("Down({btn:?})"),
        MouseEventKind::Up(btn) => format!("Up({btn:?})"),
        MouseEventKind::Drag(btn) => format!("Drag({btn:?})"),
        MouseEventKind::Moved => "Moved".into(),
        MouseEventKind::ScrollDown => "ScrollDown".into(),
        MouseEventKind::ScrollUp => "ScrollUp".into(),
        MouseEventKind::ScrollLeft => "ScrollLeft".into(),
        MouseEventKind::ScrollRight => "ScrollRight".into(),
    }
}

/// Build a `StateContext` from the current `VizApp` state.
pub fn capture_state_context(app: &super::state::VizApp) -> StateContext {
    let focused_panel = match app.focused_panel {
        FocusedPanel::Graph => "graph",
        FocusedPanel::RightPanel => "right_panel",
    };
    let right_panel_tab = if app.right_panel_visible {
        Some(match app.right_panel_tab {
            RightPanelTab::Chat => "chat",
            RightPanelTab::Detail => "detail",
            RightPanelTab::Log => "log",
            RightPanelTab::Messages => "messages",
            RightPanelTab::Agency => "agency",
            RightPanelTab::Config => "config",
            RightPanelTab::Files => "files",
            RightPanelTab::CoordLog => "coord_log",
            RightPanelTab::Firehose => "firehose",
            RightPanelTab::Output => "output",
            RightPanelTab::Dashboard => "dashboard",
        })
    } else {
        None
    };
    let selected_task = app
        .selected_task_idx
        .and_then(|idx| app.task_order.get(idx))
        .cloned();

    StateContext {
        focused_panel,
        right_panel_tab,
        selected_task,
        right_panel_visible: app.right_panel_visible,
        search_active: app.search_active,
        show_help: app.show_help,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use std::io::{BufRead, BufReader};

    fn test_ctx() -> StateContext {
        StateContext {
            focused_panel: "graph",
            right_panel_tab: Some("detail"),
            selected_task: Some("my-task".into()),
            right_panel_visible: true,
            search_active: false,
            show_help: false,
        }
    }

    #[test]
    fn trace_writes_valid_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        {
            let mut tracer = EventTracer::new(&path).unwrap();
            // Key event
            tracer.record(
                &Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
                test_ctx(),
            );
            // Mouse event
            tracer.record(
                &Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: 10,
                    row: 5,
                    modifiers: KeyModifiers::NONE,
                }),
                test_ctx(),
            );
            // Resize event
            tracer.record(
                &Event::Resize(120, 40),
                test_ctx(),
            );
            tracer.flush();
        }
        // Read back and verify each line is valid JSON
        let file = std::fs::File::open(&path).unwrap();
        let reader = BufReader::new(file);
        let lines: Vec<String> = reader.lines().map(|l| l.unwrap()).collect();
        assert_eq!(lines.len(), 3);
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(v["t"].is_f64(), "timestamp should be a float");
            assert!(v["event"]["type"].is_string(), "event type should be present");
            assert_eq!(v["state"]["focused_panel"], "graph");
        }
        // Check specific event types
        let v0: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v0["event"]["type"], "Key");
        assert_eq!(v0["event"]["code"], "j");

        let v1: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(v1["event"]["type"], "Mouse");
        assert_eq!(v1["event"]["row"], 5);
        assert_eq!(v1["event"]["col"], 10);

        let v2: serde_json::Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(v2["event"]["type"], "Resize");
        assert_eq!(v2["event"]["width"], 120);
        assert_eq!(v2["event"]["height"], 40);
    }

    #[test]
    fn trace_timestamps_are_monotonic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        {
            let mut tracer = EventTracer::new(&path).unwrap();
            for _ in 0..10 {
                tracer.record(
                    &Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
                    test_ctx(),
                );
            }
            tracer.flush();
        }
        let file = std::fs::File::open(&path).unwrap();
        let reader = BufReader::new(file);
        let mut prev_t = -1.0_f64;
        for line in reader.lines() {
            let v: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
            let t = v["t"].as_f64().unwrap();
            assert!(t >= prev_t, "timestamps must be monotonically non-decreasing");
            prev_t = t;
        }
    }

    #[test]
    fn format_modifiers_empty_for_none() {
        assert_eq!(format_modifiers(KeyModifiers::NONE), "");
    }

    #[test]
    fn format_modifiers_combined() {
        let mods = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        let s = format_modifiers(mods);
        assert!(s.contains("Shift"));
        assert!(s.contains("Ctrl"));
    }
}
