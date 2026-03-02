pub mod event;
pub mod render;
pub mod state;

use std::io;
use std::path::PathBuf;

use anyhow::Result;
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

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

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;

    let mut terminal = ratatui::init();
    let mut app = VizApp::new(workgraph_dir, viz_options, mouse_override);
    let result = event::run_event_loop(&mut terminal, &mut app);

    let _ = restore_terminal();
    ratatui::restore();

    result
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableBracketedPaste)?;
    Ok(())
}
