//! Render a TUI event trace into an asciinema .cast file.
//!
//! Reads a JSONL trace file produced by `wg tui --trace`, replays the events
//! against the TUI in headless mode (via ratatui TestBackend), applies time
//! compression to collapse idle periods, and writes an asciinema v2 .cast file.

use anyhow::{Context, Result, bail};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::prelude::*;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::commands::viz::VizOptions;
use crate::tui::viz_viewer::{render, state::VizApp};

// ── Trace file parsing ──────────────────────────────────────────────────────

#[derive(Deserialize)]
struct TraceEntry {
    t: f64,
    event: TracedEventData,
    #[allow(dead_code)]
    state: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum TracedEventData {
    Key { code: String, modifiers: String },
    Mouse { kind: String, row: u16, col: u16 },
    Paste { len: usize },
    Resize { width: u16, height: u16 },
    FocusGained,
    FocusLost,
}

fn parse_trace(path: &Path) -> Result<Vec<TraceEntry>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot open trace: {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut entries = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("read error at line {}", i + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: TraceEntry = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSON at line {}", i + 1))?;
        entries.push(entry);
    }
    Ok(entries)
}

// ── Event reconstruction ────────────────────────────────────────────────────

fn reconstruct_event(data: &TracedEventData) -> Option<Event> {
    match data {
        TracedEventData::Key { code, modifiers } => {
            let kc = parse_key_code(code)?;
            let mods = parse_modifiers(modifiers);
            Some(Event::Key(KeyEvent {
                code: kc,
                modifiers: mods,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            }))
        }
        TracedEventData::Mouse { kind, row, col } => {
            let mk = parse_mouse_kind(kind)?;
            Some(Event::Mouse(MouseEvent {
                kind: mk,
                column: *col,
                row: *row,
                modifiers: KeyModifiers::NONE,
            }))
        }
        TracedEventData::Paste { len } => {
            // We only have the length, not the text. Use placeholder.
            let text = "x".repeat(*len);
            Some(Event::Paste(text))
        }
        TracedEventData::Resize { width, height } => Some(Event::Resize(*width, *height)),
        TracedEventData::FocusGained => Some(Event::FocusGained),
        TracedEventData::FocusLost => Some(Event::FocusLost),
    }
}

fn parse_key_code(s: &str) -> Option<KeyCode> {
    // Single character
    if s.len() == 1 {
        return Some(KeyCode::Char(s.chars().next().unwrap()));
    }
    // Function keys
    if let Some(rest) = s.strip_prefix('F')
        && let Ok(n) = rest.parse::<u8>()
    {
        return Some(KeyCode::F(n));
    }
    match s {
        "Backspace" => Some(KeyCode::Backspace),
        "Enter" => Some(KeyCode::Enter),
        "Left" => Some(KeyCode::Left),
        "Right" => Some(KeyCode::Right),
        "Up" => Some(KeyCode::Up),
        "Down" => Some(KeyCode::Down),
        "Home" => Some(KeyCode::Home),
        "End" => Some(KeyCode::End),
        "PageUp" => Some(KeyCode::PageUp),
        "PageDown" => Some(KeyCode::PageDown),
        "Tab" => Some(KeyCode::Tab),
        "BackTab" => Some(KeyCode::BackTab),
        "Delete" => Some(KeyCode::Delete),
        "Insert" => Some(KeyCode::Insert),
        "Esc" => Some(KeyCode::Esc),
        "CapsLock" => Some(KeyCode::CapsLock),
        "ScrollLock" => Some(KeyCode::ScrollLock),
        "NumLock" => Some(KeyCode::NumLock),
        "PrintScreen" => Some(KeyCode::PrintScreen),
        "Pause" => Some(KeyCode::Pause),
        "Menu" => Some(KeyCode::Menu),
        "KeypadBegin" => Some(KeyCode::KeypadBegin),
        _ => None,
    }
}

fn parse_modifiers(s: &str) -> KeyModifiers {
    if s.is_empty() {
        return KeyModifiers::NONE;
    }
    let mut mods = KeyModifiers::NONE;
    for part in s.split('+') {
        match part.trim() {
            "Shift" => mods |= KeyModifiers::SHIFT,
            "Ctrl" => mods |= KeyModifiers::CONTROL,
            "Alt" => mods |= KeyModifiers::ALT,
            "Super" => mods |= KeyModifiers::SUPER,
            "Hyper" => mods |= KeyModifiers::HYPER,
            "Meta" => mods |= KeyModifiers::META,
            _ => {}
        }
    }
    mods
}

fn parse_mouse_kind(s: &str) -> Option<MouseEventKind> {
    match s {
        "Moved" => Some(MouseEventKind::Moved),
        "ScrollDown" => Some(MouseEventKind::ScrollDown),
        "ScrollUp" => Some(MouseEventKind::ScrollUp),
        "ScrollLeft" => Some(MouseEventKind::ScrollLeft),
        "ScrollRight" => Some(MouseEventKind::ScrollRight),
        _ => {
            // Parse "Down(Left)", "Up(Right)", "Drag(Middle)" etc.
            if let Some(rest) = s.strip_prefix("Down(") {
                let btn = parse_mouse_button(rest.trim_end_matches(')'))?;
                Some(MouseEventKind::Down(btn))
            } else if let Some(rest) = s.strip_prefix("Up(") {
                let btn = parse_mouse_button(rest.trim_end_matches(')'))?;
                Some(MouseEventKind::Up(btn))
            } else if let Some(rest) = s.strip_prefix("Drag(") {
                let btn = parse_mouse_button(rest.trim_end_matches(')'))?;
                Some(MouseEventKind::Drag(btn))
            } else {
                None
            }
        }
    }
}

fn parse_mouse_button(s: &str) -> Option<MouseButton> {
    match s {
        "Left" => Some(MouseButton::Left),
        "Right" => Some(MouseButton::Right),
        "Middle" => Some(MouseButton::Middle),
        _ => None,
    }
}

// ── Time compression ────────────────────────────────────────────────────────

/// Parse a compress-idle spec like "5:2" into (threshold, target).
pub fn parse_compress_idle(s: &str) -> Result<(f64, f64)> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        bail!("--compress-idle must be threshold:target (e.g. 5:2)");
    }
    let threshold: f64 = parts[0]
        .parse()
        .context("invalid threshold in --compress-idle")?;
    let target: f64 = parts[1]
        .parse()
        .context("invalid target in --compress-idle")?;
    if threshold <= 0.0 || target <= 0.0 {
        bail!("--compress-idle values must be positive");
    }
    Ok((threshold, target))
}

/// Compute compressed timestamps for a sequence of events.
///
/// Gaps exceeding `idle_threshold` seconds are compressed to `compress_to`.
/// If `target_duration` is set, an additional uniform scale is applied so
/// the total recording duration matches the target.
fn compress_timestamps(
    raw_times: &[f64],
    idle_threshold: f64,
    compress_to: f64,
    target_duration: Option<f64>,
) -> Vec<f64> {
    if raw_times.is_empty() {
        return Vec::new();
    }

    let mut compressed = Vec::with_capacity(raw_times.len());
    compressed.push(0.0);

    for i in 1..raw_times.len() {
        let gap = raw_times[i] - raw_times[i - 1];
        let adjusted = if gap > idle_threshold {
            compress_to
        } else {
            gap
        };
        compressed.push(compressed[i - 1] + adjusted);
    }

    // Apply target duration scaling if requested.
    if let Some(target) = target_duration {
        let total = *compressed.last().unwrap_or(&0.0);
        if total > 0.0 {
            let scale = target / total;
            for t in &mut compressed {
                *t *= scale;
            }
        }
    }

    compressed
}

// ── Buffer → ANSI conversion ────────────────────────────────────────────────

/// Convert a ratatui Buffer to an ANSI escape sequence string.
///
/// Produces a full-screen repaint: cursor home + clear, then every cell
/// with its style. This is intentionally simple (not diff-based) since each
/// .cast entry represents a complete frame.
fn buffer_to_ansi(buf: &Buffer) -> String {
    let area = buf.area;
    // Pre-allocate generously: ~20 bytes per cell for style + content
    let mut out = String::with_capacity((area.width as usize) * (area.height as usize) * 20);

    // Clear screen and move cursor home
    out.push_str("\x1b[H\x1b[2J");

    for y in area.top()..area.bottom() {
        if y > area.top() {
            out.push_str("\r\n");
        }

        let mut prev_fg: Option<Color> = None;
        let mut prev_bg: Option<Color> = None;
        let mut prev_mods = Modifier::empty();

        for x in area.left()..area.right() {
            let cell = &buf[(x, y)];
            let symbol = cell.symbol();

            // Skip continuation cells of wide characters
            if symbol.is_empty() {
                continue;
            }

            let fg = cell.fg;
            let bg = cell.bg;
            let mods = cell.modifier;

            // Emit style change if anything differs
            if Some(fg) != prev_fg || Some(bg) != prev_bg || mods != prev_mods {
                out.push_str("\x1b[0"); // reset, then add attributes
                push_modifier_codes(&mut out, mods);
                push_fg_code(&mut out, fg);
                push_bg_code(&mut out, bg);
                out.push('m');

                prev_fg = Some(fg);
                prev_bg = Some(bg);
                prev_mods = mods;
            }

            out.push_str(symbol);
        }
    }

    // Final reset
    out.push_str("\x1b[0m");
    out
}

fn push_modifier_codes(out: &mut String, mods: Modifier) {
    if mods.contains(Modifier::BOLD) {
        out.push_str(";1");
    }
    if mods.contains(Modifier::DIM) {
        out.push_str(";2");
    }
    if mods.contains(Modifier::ITALIC) {
        out.push_str(";3");
    }
    if mods.contains(Modifier::UNDERLINED) {
        out.push_str(";4");
    }
    if mods.contains(Modifier::SLOW_BLINK) {
        out.push_str(";5");
    }
    if mods.contains(Modifier::RAPID_BLINK) {
        out.push_str(";6");
    }
    if mods.contains(Modifier::REVERSED) {
        out.push_str(";7");
    }
    if mods.contains(Modifier::HIDDEN) {
        out.push_str(";8");
    }
    if mods.contains(Modifier::CROSSED_OUT) {
        out.push_str(";9");
    }
}

fn push_fg_code(out: &mut String, color: Color) {
    match color {
        Color::Reset => {} // default fg, no code needed after reset
        Color::Black => out.push_str(";30"),
        Color::Red => out.push_str(";31"),
        Color::Green => out.push_str(";32"),
        Color::Yellow => out.push_str(";33"),
        Color::Blue => out.push_str(";34"),
        Color::Magenta => out.push_str(";35"),
        Color::Cyan => out.push_str(";36"),
        Color::Gray => out.push_str(";37"),
        Color::DarkGray => out.push_str(";90"),
        Color::LightRed => out.push_str(";91"),
        Color::LightGreen => out.push_str(";92"),
        Color::LightYellow => out.push_str(";93"),
        Color::LightBlue => out.push_str(";94"),
        Color::LightMagenta => out.push_str(";95"),
        Color::LightCyan => out.push_str(";96"),
        Color::White => out.push_str(";97"),
        Color::Indexed(i) => {
            out.push_str(";38;5;");
            out.push_str(&i.to_string());
        }
        Color::Rgb(r, g, b) => {
            out.push_str(";38;2;");
            out.push_str(&r.to_string());
            out.push(';');
            out.push_str(&g.to_string());
            out.push(';');
            out.push_str(&b.to_string());
        }
    }
}

fn push_bg_code(out: &mut String, color: Color) {
    match color {
        Color::Reset => {}
        Color::Black => out.push_str(";40"),
        Color::Red => out.push_str(";41"),
        Color::Green => out.push_str(";42"),
        Color::Yellow => out.push_str(";43"),
        Color::Blue => out.push_str(";44"),
        Color::Magenta => out.push_str(";45"),
        Color::Cyan => out.push_str(";46"),
        Color::Gray => out.push_str(";47"),
        Color::DarkGray => out.push_str(";100"),
        Color::LightRed => out.push_str(";101"),
        Color::LightGreen => out.push_str(";102"),
        Color::LightYellow => out.push_str(";103"),
        Color::LightBlue => out.push_str(";104"),
        Color::LightMagenta => out.push_str(";105"),
        Color::LightCyan => out.push_str(";106"),
        Color::White => out.push_str(";107"),
        Color::Indexed(i) => {
            out.push_str(";48;5;");
            out.push_str(&i.to_string());
        }
        Color::Rgb(r, g, b) => {
            out.push_str(";48;2;");
            out.push_str(&r.to_string());
            out.push(';');
            out.push_str(&g.to_string());
            out.push(';');
            out.push_str(&b.to_string());
        }
    }
}

// ── Asciinema v2 .cast output ───────────────────────────────────────────────

#[derive(Serialize)]
struct CastHeader {
    version: u32,
    width: u16,
    height: u16,
    timestamp: i64,
    env: CastEnv,
    #[serde(skip_serializing_if = "Option::is_none")]
    idle_time_limit: Option<f64>,
}

#[derive(Serialize)]
struct CastEnv {
    #[serde(rename = "TERM")]
    term: String,
    #[serde(rename = "SHELL")]
    shell: String,
}

fn write_cast_header(w: &mut impl Write, width: u16, height: u16) -> Result<()> {
    let header = CastHeader {
        version: 2,
        width,
        height,
        timestamp: chrono::Utc::now().timestamp(),
        env: CastEnv {
            term: "xterm-256color".to_string(),
            shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string()),
        },
        idle_time_limit: Some(2.0),
    };
    serde_json::to_writer(&mut *w, &header)?;
    w.write_all(b"\n")?;
    Ok(())
}

fn write_cast_entry(w: &mut impl Write, timestamp: f64, data: &str) -> Result<()> {
    // Asciinema v2 entry: [timestamp, "o", "data"]
    write!(w, "[{:.6}, \"o\", ", timestamp)?;
    serde_json::to_writer(&mut *w, data)?;
    w.write_all(b"]\n")?;
    Ok(())
}

// ── Main entry point ────────────────────────────────────────────────────────

pub fn run(
    workgraph_dir: &Path,
    trace_path: &Path,
    output_path: &Path,
    compress_idle: &str,
    target_duration: Option<f64>,
    width: u16,
    height: u16,
) -> Result<()> {
    let (idle_threshold, compress_to) = parse_compress_idle(compress_idle)?;

    // 1. Parse trace file
    let entries = parse_trace(trace_path)?;
    if entries.is_empty() {
        bail!("trace file is empty: {}", trace_path.display());
    }

    eprintln!(
        "Rendering {} trace events from {}",
        entries.len(),
        trace_path.display()
    );

    // 2. Compute compressed timestamps
    let raw_times: Vec<f64> = entries.iter().map(|e| e.t).collect();
    let compressed_times =
        compress_timestamps(&raw_times, idle_threshold, compress_to, target_duration);

    // 3. Set up headless TUI
    let viz_options = VizOptions {
        show_internal: true,
        ..VizOptions::default()
    };
    let mut app = VizApp::new(PathBuf::from(workgraph_dir), viz_options, Some(false));
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend)?;

    // 4. Open output file
    let mut out_file = std::io::BufWriter::new(
        std::fs::File::create(output_path)
            .with_context(|| format!("cannot create output: {}", output_path.display()))?,
    );
    write_cast_header(&mut out_file, width, height)?;

    // 5. Render initial frame (t=0, before any events)
    terminal.draw(|frame| render::draw(frame, &mut app))?;
    let initial_ansi = buffer_to_ansi(terminal.backend().buffer());
    write_cast_entry(&mut out_file, 0.0, &initial_ansi)?;

    // 6. Replay each event
    let mut rendered = 0;
    for (i, entry) in entries.iter().enumerate() {
        if let Some(ev) = reconstruct_event(&entry.event) {
            crate::tui::viz_viewer::event::dispatch_event(&mut app, ev);
            terminal.draw(|frame| render::draw(frame, &mut app))?;
            let ansi = buffer_to_ansi(terminal.backend().buffer());

            // Use a small offset so the first event doesn't overlap with the
            // initial frame at t=0.
            let t = compressed_times[i].max(0.01);
            write_cast_entry(&mut out_file, t, &ansi)?;
            rendered += 1;
        }

        if app.should_quit {
            break;
        }
    }

    out_file.flush()?;

    let total_duration = compressed_times.last().copied().unwrap_or(0.0);
    let original_duration = raw_times.last().copied().unwrap_or(0.0);
    eprintln!(
        "Wrote {} frames to {} ({:.1}s compressed from {:.1}s original)",
        rendered + 1, // +1 for initial frame
        output_path.display(),
        total_duration,
        original_duration,
    );

    Ok(())
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_compress_idle_valid() {
        let (t, c) = parse_compress_idle("5:2").unwrap();
        assert!((t - 5.0).abs() < f64::EPSILON);
        assert!((c - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_compress_idle_decimals() {
        let (t, c) = parse_compress_idle("3.5:1.5").unwrap();
        assert!((t - 3.5).abs() < f64::EPSILON);
        assert!((c - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_compress_idle_invalid_format() {
        assert!(parse_compress_idle("5").is_err());
        assert!(parse_compress_idle("5:2:1").is_err());
        assert!(parse_compress_idle("abc:2").is_err());
    }

    #[test]
    fn parse_compress_idle_negative() {
        assert!(parse_compress_idle("-1:2").is_err());
        assert!(parse_compress_idle("5:-1").is_err());
    }

    #[test]
    fn compress_timestamps_no_idle() {
        let raw = vec![0.0, 1.0, 2.0, 3.0];
        let result = compress_timestamps(&raw, 5.0, 2.0, None);
        assert_eq!(result, vec![0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn compress_timestamps_with_idle() {
        // 0, 1, 11, 12 — the gap 1→11 (10s) exceeds threshold (5s)
        let raw = vec![0.0, 1.0, 11.0, 12.0];
        let result = compress_timestamps(&raw, 5.0, 2.0, None);
        assert_eq!(result, vec![0.0, 1.0, 3.0, 4.0]);
    }

    #[test]
    fn compress_timestamps_with_target_duration() {
        let raw = vec![0.0, 1.0, 2.0, 4.0];
        let result = compress_timestamps(&raw, 5.0, 2.0, Some(8.0));
        // No idle compression needed (all gaps < 5s), total = 4s
        // Scale factor = 8.0 / 4.0 = 2.0
        assert!((result[0] - 0.0).abs() < 1e-9);
        assert!((result[1] - 2.0).abs() < 1e-9);
        assert!((result[2] - 4.0).abs() < 1e-9);
        assert!((result[3] - 8.0).abs() < 1e-9);
    }

    #[test]
    fn compress_timestamps_empty() {
        assert!(compress_timestamps(&[], 5.0, 2.0, None).is_empty());
    }

    #[test]
    fn parse_key_code_chars() {
        assert_eq!(parse_key_code("j"), Some(KeyCode::Char('j')));
        assert_eq!(parse_key_code("A"), Some(KeyCode::Char('A')));
    }

    #[test]
    fn parse_key_code_special() {
        assert_eq!(parse_key_code("Enter"), Some(KeyCode::Enter));
        assert_eq!(parse_key_code("Esc"), Some(KeyCode::Esc));
        assert_eq!(parse_key_code("Tab"), Some(KeyCode::Tab));
        assert_eq!(parse_key_code("Up"), Some(KeyCode::Up));
        assert_eq!(parse_key_code("Down"), Some(KeyCode::Down));
        assert_eq!(parse_key_code("Left"), Some(KeyCode::Left));
        assert_eq!(parse_key_code("Right"), Some(KeyCode::Right));
        assert_eq!(parse_key_code("Backspace"), Some(KeyCode::Backspace));
    }

    #[test]
    fn parse_key_code_function() {
        assert_eq!(parse_key_code("F1"), Some(KeyCode::F(1)));
        assert_eq!(parse_key_code("F12"), Some(KeyCode::F(12)));
    }

    #[test]
    fn parse_modifiers_empty() {
        assert_eq!(parse_modifiers(""), KeyModifiers::NONE);
    }

    #[test]
    fn parse_modifiers_combined() {
        let mods = parse_modifiers("Shift+Ctrl");
        assert!(mods.contains(KeyModifiers::SHIFT));
        assert!(mods.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn parse_mouse_kind_scroll() {
        assert_eq!(
            parse_mouse_kind("ScrollDown"),
            Some(MouseEventKind::ScrollDown)
        );
        assert_eq!(parse_mouse_kind("ScrollUp"), Some(MouseEventKind::ScrollUp));
    }

    #[test]
    fn parse_mouse_kind_buttons() {
        assert_eq!(
            parse_mouse_kind("Down(Left)"),
            Some(MouseEventKind::Down(MouseButton::Left))
        );
        assert_eq!(
            parse_mouse_kind("Up(Right)"),
            Some(MouseEventKind::Up(MouseButton::Right))
        );
        assert_eq!(
            parse_mouse_kind("Drag(Middle)"),
            Some(MouseEventKind::Drag(MouseButton::Middle))
        );
    }

    #[test]
    fn reconstruct_key_event() {
        let data = TracedEventData::Key {
            code: "j".to_string(),
            modifiers: String::new(),
        };
        let ev = reconstruct_event(&data).unwrap();
        match ev {
            Event::Key(key) => {
                assert_eq!(key.code, KeyCode::Char('j'));
                assert_eq!(key.modifiers, KeyModifiers::NONE);
                assert_eq!(key.kind, KeyEventKind::Press);
            }
            _ => panic!("expected Key event"),
        }
    }

    #[test]
    fn reconstruct_resize_event() {
        let data = TracedEventData::Resize {
            width: 120,
            height: 40,
        };
        let ev = reconstruct_event(&data).unwrap();
        assert!(matches!(ev, Event::Resize(120, 40)));
    }

    #[test]
    fn buffer_to_ansi_basic() {
        let backend = TestBackend::new(5, 2);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let area = frame.area();
                let text = ratatui::widgets::Paragraph::new("hello");
                frame.render_widget(text, area);
            })
            .unwrap();

        let ansi = buffer_to_ansi(terminal.backend().buffer());

        // Should start with clear screen
        assert!(ansi.starts_with("\x1b[H\x1b[2J"));
        // Should contain "hello"
        assert!(ansi.contains("hello"));
        // Should end with reset
        assert!(ansi.ends_with("\x1b[0m"));
    }

    #[test]
    fn buffer_to_ansi_styled() {
        let backend = TestBackend::new(10, 1);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal
            .draw(|frame| {
                let area = frame.area();
                let text = ratatui::widgets::Paragraph::new(ratatui::text::Line::from(vec![
                    Span::styled("red", Style::default().fg(Color::Red)),
                ]));
                frame.render_widget(text, area);
            })
            .unwrap();

        let ansi = buffer_to_ansi(terminal.backend().buffer());

        // Should contain red foreground ANSI code (31)
        assert!(ansi.contains(";31"));
        assert!(ansi.contains("red"));
    }

    #[test]
    fn write_cast_header_valid_json() {
        let mut buf = Vec::new();
        write_cast_header(&mut buf, 120, 36).unwrap();
        let line = String::from_utf8(buf).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["version"], 2);
        assert_eq!(v["width"], 120);
        assert_eq!(v["height"], 36);
    }

    #[test]
    fn write_cast_entry_valid_json() {
        let mut buf = Vec::new();
        write_cast_entry(&mut buf, 1.5, "hello\x1b[0m").unwrap();
        let line = String::from_utf8(buf).unwrap();
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert!(v[0].is_f64());
        assert_eq!(v[1], "o");
        assert!(v[2].is_string());
    }

    #[test]
    fn parse_trace_valid_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        std::fs::write(
            &path,
            r#"{"t":0.0,"event":{"type":"Key","code":"j","modifiers":""},"state":{"focused_panel":"graph","right_panel_tab":"detail","selected_task":"t1","right_panel_visible":true,"search_active":false,"show_help":false}}
{"t":1.5,"event":{"type":"Key","code":"k","modifiers":"Ctrl"},"state":{"focused_panel":"graph","right_panel_tab":"detail","selected_task":"t1","right_panel_visible":true,"search_active":false,"show_help":false}}
"#,
        )
        .unwrap();

        let entries = parse_trace(&path).unwrap();
        assert_eq!(entries.len(), 2);
        assert!((entries[0].t - 0.0).abs() < f64::EPSILON);
        assert!((entries[1].t - 1.5).abs() < f64::EPSILON);
    }
}
