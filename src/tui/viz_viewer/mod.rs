pub mod event;
pub mod file_browser;
pub mod file_browser_render;
pub mod render;
pub mod state;

#[cfg(test)]
mod editor_tests;

use std::io;
use std::path::PathBuf;

use anyhow::{Context, Result};
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        supports_keyboard_enhancement,
    },
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use self::state::VizApp;
use crate::commands::viz::VizOptions;

/// Run the viz viewer TUI.
///
/// `mouse_override`: `Some(false)` to force mouse off (--no-mouse),
/// `None` for auto-detection (disabled in tmux splits).
pub fn run(
    workgraph_dir: PathBuf,
    viz_options: VizOptions,
    mouse_override: Option<bool>,
) -> Result<()> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = restore_terminal();
        original_hook(panic_info);
    }));

    enable_raw_mode().context(
        "failed to enable raw mode — is this an interactive terminal?\n\
         Hint: `wg tui` requires a real terminal (not a pipe or agent context)",
    )?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;

    // Enable kitty keyboard protocol if supported — this lets us distinguish
    // Shift+Enter from Enter (and other modified special keys).
    let has_keyboard_enhancement = supports_keyboard_enhancement().unwrap_or(false);
    if has_keyboard_enhancement {
        let _ = execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
    }

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("failed to create terminal for TUI")?;
    let mut app = VizApp::new(workgraph_dir, viz_options, mouse_override);
    app.has_keyboard_enhancement = has_keyboard_enhancement;
    let result = event::run_event_loop(&mut terminal, &mut app);

    let _ = restore_terminal();

    result
}

fn restore_terminal() -> Result<()> {
    use io::Write;
    // Best-effort cleanup: don't short-circuit on individual failures
    // so that later steps still run even if an earlier one fails.
    let r1 = disable_raw_mode();
    // Disable mouse modes with raw escape sequences (matching event.rs)
    let r2 = io::stdout().write_all(b"\x1b[?1006l\x1b[?1000l");
    // Pop kitty keyboard enhancement (no-op if it wasn't pushed).
    let r3 = execute!(io::stdout(), PopKeyboardEnhancementFlags);
    let r4 = execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste);
    r1?;
    r2?;
    let _ = r3; // Ignore error — may not have been pushed.
    r4?;
    Ok(())
}
