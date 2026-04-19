//! `wg tui-pty` — PTY-embedded `wg nex` in a minimal ratatui pane.
//!
//! Spawns `wg nex` (plus any pass-through args) as a child process
//! attached to a pseudo-terminal, renders the PTY cell buffer
//! fullscreen via `tui-term`'s widget, and forwards key events to
//! the child's stdin. The embedded nex is a real terminal process —
//! it does rustyline line editing, streams assistant text, draws
//! tool boxes, handles Ctrl-C, renders slash-command output, all in
//! the same bytes it would produce in a real terminal.
//!
//! We do NOT reimplement any nex features here. This is strictly
//! infrastructure: spawn, render, key-forward, resize, tear down.

use std::io;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use crate::tui::pty_pane::PtyPane;

/// Poll interval for crossterm events + child-alive check. Short
/// enough that the cursor stays responsive, long enough not to burn
/// CPU. Matches typical ratatui TUI loops.
const EVENT_POLL: Duration = Duration::from_millis(33);

pub fn run(
    workgraph_dir: &Path,
    model: Option<&str>,
    endpoint: Option<&str>,
    chat_ref: Option<&str>,
    resume: Option<&str>,
) -> Result<()> {
    // Resolve the `wg` binary path. Prefer the current exe so a dev
    // running `cargo run` or a user running an installed `wg` stays
    // consistent. Fall back to "wg" in PATH.
    let wg_bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "wg".to_string());

    let mut args: Vec<&str> = vec!["nex"];
    if let Some(m) = model {
        args.push("-m");
        args.push(m);
    }
    if let Some(e) = endpoint {
        args.push("-e");
        args.push(e);
    }
    if let Some(c) = chat_ref {
        args.push("--chat");
        args.push(c);
    }
    if let Some(r) = resume {
        args.push("--resume");
        if !r.is_empty() {
            args.push(r);
        }
    }

    // Environment pass-through — propagate the workgraph dir to the
    // child so it uses the same state we were invoked with. The
    // child resolves WG_DIR at startup just like a normal `wg nex`
    // invocation would.
    let env: Vec<(String, String)> = vec![(
        "WG_DIR".to_string(),
        workgraph_dir.display().to_string(),
    )];

    // Set up the host ratatui terminal.
    let mut stdout = io::stdout();
    enable_raw_mode().context("enable_raw_mode failed")?;
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)
        .context("terminal enter failed")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("terminal init failed")?;

    // Panic hook: restore terminal on unwind so a panic doesn't
    // leave the user's shell in raw mode + alternate screen.
    let prior = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        prior(info);
    }));

    let result = run_loop(&mut terminal, &wg_bin, &args, &env);

    let _ = restore_terminal();

    result
}

fn restore_terminal() -> Result<()> {
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen, DisableBracketedPaste)?;
    disable_raw_mode()?;
    Ok(())
}

fn run_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    wg_bin: &str,
    args: &[&str],
    env: &[(String, String)],
) -> Result<()> {
    // Initial size. On some terminals (or before the first draw)
    // `terminal.size()` can report 0x0, which makes vt100::Parser
    // index into an empty grid and panic on the first byte the child
    // writes. Clamp to a sane minimum — the first real `draw` call
    // below resizes the pane to the actual frame area immediately,
    // so any "wrong" initial dims are transient.
    let initial = terminal
        .size()
        .map_err(|e| anyhow::anyhow!("terminal.size() failed: {:?}", e))?;
    let init_rows = initial.height.max(24);
    let init_cols = initial.width.max(80);
    let mut pane = PtyPane::spawn(wg_bin, args, env, init_rows, init_cols)
        .context("PTY spawn failed")?;

    loop {
        // Draw: full-screen PTY. Resize the PTY to match the drawing
        // area so the embedded nex reflows its output correctly.
        // Ratatui 0.30's Backend::Error is no longer Send+Sync, so we
        // can't use `?` directly with anyhow — map explicitly.
        terminal
            .draw(|f| {
                let area: Rect = f.area();
                let _ = pane.resize(area.height, area.width);
                pane.render(f, area);
            })
            .map_err(|e| anyhow::anyhow!("terminal.draw() failed: {:?}", e))?;

        // Child-exit check: once `wg nex` exits (user /quit, EOF,
        // max_turns, etc.) we tear down the host TUI. Check before
        // polling for more input so a pending key event doesn't
        // resurrect a dead pane.
        if !pane.is_alive() {
            return Ok(());
        }

        if !event::poll(EVENT_POLL)? {
            continue;
        }

        match event::read()? {
            Event::Key(key) => {
                // `KeyEventKind::Release` fires on some terminals when
                // keyboard enhancement is on; we only want press events
                // (otherwise every keystroke gets doubled).
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                if is_host_quit(&key) {
                    pane.kill();
                    return Ok(());
                }
                pane.send_key(key)?;
            }
            Event::Resize(cols, rows) => {
                pane.resize(rows, cols)?;
            }
            Event::Paste(text) => {
                // Bracketed paste: forward verbatim. Rustyline inside
                // the child handles the paste normally.
                pane.send_text(&text)?;
            }
            _ => {}
        }
    }
}

/// Ctrl-Q is the host-level "kill embedded nex + exit" shortcut. We
/// specifically avoid hijacking anything the embedded nex wants:
/// Ctrl-C is cancel-in-nex, Ctrl-D is EOF-in-nex, Esc is for
/// rustyline's vi mode, etc. Ctrl-Q is the safest unused slot.
fn is_host_quit(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_q_triggers_host_quit() {
        let k = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert!(is_host_quit(&k));
    }

    #[test]
    fn plain_q_does_not_trigger_host_quit() {
        let k = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(!is_host_quit(&k));
    }

    #[test]
    fn ctrl_c_does_not_trigger_host_quit() {
        // Ctrl-C belongs to the embedded nex (cancel in-flight turn);
        // the host must NOT intercept it.
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(!is_host_quit(&k));
    }

    #[test]
    fn ctrl_d_does_not_trigger_host_quit() {
        // Ctrl-D is EOF for rustyline inside nex.
        let k = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(!is_host_quit(&k));
    }
}
