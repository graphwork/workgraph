use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::DefaultTerminal;

use super::render;
use super::state::VizApp;

/// Input poll timeout — short for responsive scrolling.
const INPUT_POLL: Duration = Duration::from_millis(50);

pub fn run_event_loop(terminal: &mut DefaultTerminal, app: &mut VizApp) -> Result<()> {
    loop {
        app.maybe_refresh();
        terminal.draw(|frame| render::draw(frame, app))?;

        if event::poll(INPUT_POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(app, key.code, key.modifiers);
                }
                Event::Mouse(mouse) => {
                    handle_mouse(app, mouse.kind);
                }
                Event::Resize(_, _) => {} // re-render on next iteration
                _ => {}
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn handle_key(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    // Help overlay intercepts all keys when shown
    if app.show_help {
        match code {
            KeyCode::Char('?') | KeyCode::Esc | KeyCode::Char('q') => app.show_help = false,
            _ => {} // swallow all other keys while help is shown
        }
    } else if app.search_active {
        handle_search_input(app, code, modifiers);
    } else {
        handle_normal_key(app, code, modifiers);
    }
}

fn handle_search_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Esc => {
            // Cancel search: clear everything, restore full view.
            app.clear_search();
        }
        KeyCode::Enter => {
            // Accept search: exit search mode, show all lines, keep highlights.
            if app.search_input.is_empty() {
                app.clear_search();
            } else {
                app.accept_search();
            }
        }
        KeyCode::Backspace | KeyCode::Delete => {
            app.search_input.pop();
            app.update_search();
        }

        // Ctrl-U clears the search input (like in vim/shell).
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.search_input.clear();
            app.update_search();
        }

        // Ctrl-C quits even from search mode.
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }

        // Regular character input.
        KeyCode::Char(c) => {
            app.search_input.push(c);
            app.update_search();
        }

        // Navigate between matches with Tab/Shift-Tab while typing.
        KeyCode::BackTab => app.prev_match(),
        KeyCode::Tab => app.next_match(),

        // Horizontal scroll with arrow keys while typing.
        KeyCode::Left => app.scroll.scroll_left(4),
        KeyCode::Right => app.scroll.scroll_right(4),

        // Scroll the filtered view with Up/Down while typing.
        KeyCode::Up => app.scroll.scroll_up(1),
        KeyCode::Down => app.scroll.scroll_down(1),

        _ => {}
    }
}

fn handle_normal_key(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        // Help overlay
        KeyCode::Char('?') => app.show_help = true,

        // Quit
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Esc => {
            // If there's an active search, clear it; otherwise quit.
            if app.has_active_search() {
                app.clear_search();
            } else {
                app.should_quit = true;
            }
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }

        // Search
        KeyCode::Char('/') => {
            app.search_active = true;
            app.search_input.clear();
            app.fuzzy_matches.clear();
            app.current_match = None;
            app.filtered_indices = None;
            app.update_scroll_bounds();
        }

        // Navigate between matches.
        KeyCode::Char('n') | KeyCode::Tab => app.next_match(),
        KeyCode::Char('N') | KeyCode::BackTab => app.prev_match(),

        // Vertical scroll
        KeyCode::Up | KeyCode::Char('k') => app.scroll.scroll_up(1),
        KeyCode::Down | KeyCode::Char('j') => app.scroll.scroll_down(1),
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => app.scroll.page_up(),
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => app.scroll.page_down(),
        KeyCode::PageUp => app.scroll.page_up(),
        KeyCode::PageDown => app.scroll.page_down(),

        // Jump to top/bottom
        KeyCode::Char('g') => app.scroll.go_top(),
        KeyCode::Char('G') => app.scroll.go_bottom(),

        // Manual refresh
        KeyCode::Char('r') => app.force_refresh(),

        // Horizontal scroll
        KeyCode::Left | KeyCode::Char('h') => app.scroll.scroll_left(4),
        KeyCode::Right | KeyCode::Char('l') => app.scroll.scroll_right(4),

        _ => {}
    }
}

fn handle_mouse(app: &mut VizApp, kind: MouseEventKind) {
    match kind {
        MouseEventKind::ScrollUp => app.scroll.scroll_up(3),
        MouseEventKind::ScrollDown => app.scroll.scroll_down(3),
        _ => {}
    }
}
