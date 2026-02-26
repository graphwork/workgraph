pub mod state;
pub mod event;
pub mod render;

use std::io;
use std::path::PathBuf;

use anyhow::Result;
use crossterm::{
    event::{EnableMouseCapture, DisableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};

use crate::commands::viz::VizOptions;
use self::state::VizApp;

/// Run the viz viewer TUI.
pub fn run(workgraph_dir: PathBuf, viz_options: VizOptions) -> Result<()> {
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = restore_terminal();
        original_hook(panic_info);
    }));

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

    let mut terminal = ratatui::init();
    let mut app = VizApp::new(workgraph_dir, viz_options);
    let result = event::run_event_loop(&mut terminal, &mut app);

    let _ = restore_terminal();
    ratatui::restore();

    result
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    Ok(())
}
