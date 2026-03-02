use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::execute;
use ratatui::DefaultTerminal;
use ratatui::layout::Position;

use super::render;
use super::state::{
    CommandEffect, ConfirmAction, FocusedPanel, InputMode, RightPanelTab, TaskFormField,
    TextPromptAction, VizApp,
};

/// Input poll timeout — short for responsive scrolling.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// Apply the current mouse capture state to the terminal.
fn set_mouse_capture(enabled: bool) -> Result<()> {
    if enabled {
        execute!(io::stdout(), EnableMouseCapture)?;
    } else {
        execute!(io::stdout(), DisableMouseCapture)?;
    }
    Ok(())
}

pub fn run_event_loop(terminal: &mut DefaultTerminal, app: &mut VizApp) -> Result<()> {
    // Set initial mouse capture state
    set_mouse_capture(app.mouse_enabled)?;

    let result = run_event_loop_inner(terminal, app);

    // Always disable mouse capture on exit
    let _ = set_mouse_capture(false);

    result
}

fn run_event_loop_inner(terminal: &mut DefaultTerminal, app: &mut VizApp) -> Result<()> {
    loop {
        app.maybe_refresh();
        app.drain_commands();
        terminal.draw(|frame| render::draw(frame, app))?;

        if event::poll(INPUT_POLL)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    handle_key(app, key.code, key.modifiers);
                }
                Event::Paste(text) => {
                    handle_paste(app, &text);
                }
                Event::Mouse(mouse) if app.mouse_enabled => {
                    handle_mouse(app, mouse.kind, mouse.row, mouse.column);
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
        return;
    }

    // Dispatch based on input mode
    match &app.input_mode {
        InputMode::Search => handle_search_input(app, code, modifiers),
        InputMode::TaskForm => handle_task_form_input(app, code, modifiers),
        InputMode::Confirm(_) => handle_confirm_input(app, code),
        InputMode::TextPrompt(_) => handle_text_prompt_input(app, code, modifiers),
        InputMode::ChatInput => handle_chat_input(app, code, modifiers),
        InputMode::MessageInput => handle_message_input(app, code, modifiers),
        InputMode::Normal => {
            // Also check legacy search_active flag for backward compat
            if app.search_active {
                handle_search_input(app, code, modifiers);
            } else {
                handle_normal_key(app, code, modifiers);
            }
        }
    }
}

fn handle_paste(app: &mut VizApp, text: &str) {
    match &app.input_mode {
        InputMode::ChatInput => {
            // Insert pasted text at cursor position, preserving newlines.
            app.chat.input.insert_str(app.chat.cursor, text);
            app.chat.cursor += text.len();
        }
        InputMode::Search => {
            // Strip newlines for search — it's single-line.
            let clean: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            app.search_input.push_str(&clean);
            app.update_search();
        }
        InputMode::TextPrompt(_) => {
            let clean: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            app.text_prompt.input.push_str(&clean);
        }
        InputMode::TaskForm => {
            if let Some(form) = app.task_form.as_mut() {
                match form.active_field {
                    TaskFormField::Description => {
                        form.description.push_str(text);
                    }
                    TaskFormField::Title => {
                        let clean: String =
                            text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
                        form.title.push_str(&clean);
                    }
                    TaskFormField::Tags => {
                        let clean: String =
                            text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
                        form.tags.push_str(&clean);
                    }
                    TaskFormField::Dependencies => {
                        let clean: String =
                            text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
                        form.dep_search.push_str(&clean);
                        form.update_dep_search();
                    }
                }
            }
        }
        _ => {} // Normal/Confirm modes: ignore paste
    }
}

fn handle_search_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Esc => {
            app.clear_search();
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Enter => {
            if app.search_input.is_empty() {
                app.clear_search();
            } else {
                app.accept_search_and_jump();
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace | KeyCode::Delete => {
            app.search_input.pop();
            app.update_search();
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.search_input.clear();
            app.update_search();
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Char(c) => {
            app.search_input.push(c);
            app.update_search();
        }
        KeyCode::BackTab => app.prev_match(),
        KeyCode::Tab => app.next_match(),
        KeyCode::Left => app.scroll.scroll_left(4),
        KeyCode::Right => app.scroll.scroll_right(4),
        KeyCode::Up => app.scroll.scroll_up(1),
        KeyCode::Down => app.scroll.scroll_down(1),
        _ => {}
    }
}

fn handle_confirm_input(app: &mut VizApp, code: KeyCode) {
    let action = match &app.input_mode {
        InputMode::Confirm(a) => a.clone(),
        _ => return,
    };

    match code {
        KeyCode::Char('y') | KeyCode::Enter => {
            match action {
                ConfirmAction::MarkDone(task_id) => {
                    app.exec_command(
                        vec!["done".to_string(), task_id.clone()],
                        CommandEffect::RefreshAndNotify(format!("Marked '{}' done", task_id)),
                    );
                }
                ConfirmAction::Retry(task_id) => {
                    app.exec_command(
                        vec!["retry".to_string(), task_id.clone()],
                        CommandEffect::RefreshAndNotify(format!("Retried '{}'", task_id)),
                    );
                }
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Char('n') | KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        _ => {}
    }
}

fn handle_text_prompt_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    let action = match &app.input_mode {
        InputMode::TextPrompt(a) => a.clone(),
        _ => return,
    };

    match code {
        KeyCode::Esc => {
            app.text_prompt.input.clear();
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Enter => {
            let text = app.text_prompt.input.clone();
            app.text_prompt.input.clear();
            if text.trim().is_empty() {
                app.input_mode = InputMode::Normal;
                return;
            }
            match action {
                TextPromptAction::MarkFailed(task_id) => {
                    app.exec_command(
                        vec![
                            "fail".to_string(),
                            task_id.clone(),
                            "--reason".to_string(),
                            text,
                        ],
                        CommandEffect::RefreshAndNotify(format!("Marked '{}' failed", task_id)),
                    );
                }
                TextPromptAction::SendMessage(task_id) => {
                    app.exec_command(
                        vec![
                            "msg".to_string(),
                            "send".to_string(),
                            task_id.clone(),
                            text,
                            "--from".to_string(),
                            "tui".to_string(),
                        ],
                        CommandEffect::Notify(format!("Message sent to '{}'", task_id)),
                    );
                }
                TextPromptAction::EditDescription(task_id) => {
                    app.exec_command(
                        vec![
                            "edit".to_string(),
                            task_id.clone(),
                            "-d".to_string(),
                            text,
                        ],
                        CommandEffect::RefreshAndNotify(format!("Updated '{}'", task_id)),
                    );
                }
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.text_prompt.input.pop();
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.text_prompt.input.clear();
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.text_prompt.input.clear();
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Char(c) => {
            app.text_prompt.input.push(c);
        }
        _ => {}
    }
}

fn handle_task_form_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    let form = match app.task_form.as_mut() {
        Some(f) => f,
        None => {
            app.input_mode = InputMode::Normal;
            return;
        }
    };

    // Ctrl-Enter or Ctrl-S to submit
    if (code == KeyCode::Enter && modifiers.contains(KeyModifiers::CONTROL))
        || (code == KeyCode::Char('s') && modifiers.contains(KeyModifiers::CONTROL))
    {
        app.submit_task_form();
        return;
    }

    // Esc to cancel
    if code == KeyCode::Esc {
        app.close_task_form();
        return;
    }

    // Tab to switch fields
    if code == KeyCode::Tab {
        form.active_field = form.active_field.next();
        return;
    }
    if code == KeyCode::BackTab {
        form.active_field = form.active_field.prev();
        return;
    }

    // Handle input based on active field
    match form.active_field {
        TaskFormField::Title => match code {
            KeyCode::Backspace => {
                form.title.pop();
            }
            KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
                form.title.clear();
            }
            KeyCode::Char(c) => form.title.push(c),
            _ => {}
        },
        TaskFormField::Description => match code {
            KeyCode::Char(c) => form.description.push(c),
            KeyCode::Enter => form.description.push('\n'),
            KeyCode::Backspace => {
                form.description.pop();
            }
            _ => {}
        },
        TaskFormField::Dependencies => match code {
            KeyCode::Char(c) => {
                form.dep_search.push(c);
                form.update_dep_search();
            }
            KeyCode::Backspace => {
                form.dep_search.pop();
                form.update_dep_search();
            }
            KeyCode::Enter => {
                // Select the currently highlighted dependency match
                if !form.dep_matches.is_empty() {
                    let idx = form.dep_match_idx;
                    let (id, _) = form.dep_matches[idx].clone();
                    form.selected_deps.push(id);
                    form.dep_search.clear();
                    form.dep_matches.clear();
                    form.dep_match_idx = 0;
                }
            }
            KeyCode::Up => {
                if form.dep_match_idx > 0 {
                    form.dep_match_idx -= 1;
                }
            }
            KeyCode::Down => {
                if !form.dep_matches.is_empty()
                    && form.dep_match_idx < form.dep_matches.len() - 1
                {
                    form.dep_match_idx += 1;
                }
            }
            KeyCode::Delete => {
                // Remove last selected dependency
                form.selected_deps.pop();
            }
            _ => {}
        },
        TaskFormField::Tags => match code {
            KeyCode::Char(c) => form.tags.push(c),
            KeyCode::Backspace => {
                form.tags.pop();
            }
            _ => {}
        },
    }
}

fn handle_chat_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Enter => {
            // Enter sends the message (newlines from paste are preserved in the content).
            let text = app.chat.input.clone();
            app.chat.input.clear();
            app.chat.cursor = 0;
            app.chat.input_scroll = 0;
            if !text.trim().is_empty() {
                app.send_chat_message(text);
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            if app.chat.cursor > 0 {
                let prev = prev_char_boundary(&app.chat.input, app.chat.cursor);
                app.chat.input.drain(prev..app.chat.cursor);
                app.chat.cursor = prev;
            }
        }
        KeyCode::Delete => {
            if app.chat.cursor < app.chat.input.len() {
                let next = next_char_boundary(&app.chat.input, app.chat.cursor);
                app.chat.input.drain(app.chat.cursor..next);
            }
        }
        // Ctrl+A: move to beginning of current line
        KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.chat.cursor = line_start(&app.chat.input, app.chat.cursor);
        }
        // Ctrl+E: move to end of current line
        KeyCode::Char('e') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.chat.cursor = line_end(&app.chat.input, app.chat.cursor);
        }
        // Ctrl+B: move backward one char
        KeyCode::Char('b') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.chat.cursor = prev_char_boundary(&app.chat.input, app.chat.cursor);
        }
        // Ctrl+F: move forward one char
        KeyCode::Char('f') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.chat.cursor = next_char_boundary(&app.chat.input, app.chat.cursor);
        }
        // Ctrl+K: kill to end of current line
        KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
            let end = line_end(&app.chat.input, app.chat.cursor);
            if end == app.chat.cursor && app.chat.cursor < app.chat.input.len() {
                // At end of line: delete the newline character to join lines.
                app.chat.input.drain(app.chat.cursor..app.chat.cursor + 1);
            } else {
                app.chat.input.drain(app.chat.cursor..end);
            }
        }
        // Ctrl+U: kill to beginning of current line
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            let start = line_start(&app.chat.input, app.chat.cursor);
            app.chat.input.drain(start..app.chat.cursor);
            app.chat.cursor = start;
        }
        // Ctrl+W: delete word backward
        KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
            let start = word_boundary_back(&app.chat.input, app.chat.cursor);
            app.chat.input.drain(start..app.chat.cursor);
            app.chat.cursor = start;
        }
        // Ctrl+D: delete char forward (or no-op at end)
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            if app.chat.cursor < app.chat.input.len() {
                let next = next_char_boundary(&app.chat.input, app.chat.cursor);
                app.chat.input.drain(app.chat.cursor..next);
            }
        }
        // Ctrl+C: cancel input
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.chat.input.clear();
            app.chat.cursor = 0;
            app.chat.input_scroll = 0;
            app.input_mode = InputMode::Normal;
        }
        // Arrow keys: Left/Right move within text
        KeyCode::Left => {
            app.chat.cursor = prev_char_boundary(&app.chat.input, app.chat.cursor);
        }
        KeyCode::Right => {
            app.chat.cursor = next_char_boundary(&app.chat.input, app.chat.cursor);
        }
        // Alt+Up/Down: always scroll chat history
        KeyCode::Up if modifiers.contains(KeyModifiers::ALT) => {
            app.chat.scroll = app.chat.scroll.saturating_add(1);
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::ALT) => {
            app.chat.scroll = app.chat.scroll.saturating_sub(1);
        }
        // Up/Down: navigate between lines in multi-line input
        KeyCode::Up => {
            let new_pos = move_cursor_up(&app.chat.input, app.chat.cursor);
            if new_pos == app.chat.cursor {
                // Already on first line: scroll chat history instead.
                app.chat.scroll = app.chat.scroll.saturating_add(1);
            } else {
                app.chat.cursor = new_pos;
            }
        }
        KeyCode::Down => {
            let new_pos = move_cursor_down(&app.chat.input, app.chat.cursor);
            if new_pos == app.chat.cursor {
                // Already on last line: scroll chat history instead.
                app.chat.scroll = app.chat.scroll.saturating_sub(1);
            } else {
                app.chat.cursor = new_pos;
            }
        }
        // Home/End: start/end of current line
        KeyCode::Home => {
            app.chat.cursor = line_start(&app.chat.input, app.chat.cursor);
        }
        KeyCode::End => {
            app.chat.cursor = line_end(&app.chat.input, app.chat.cursor);
        }
        KeyCode::Char(c) => {
            app.chat.input.insert(app.chat.cursor, c);
            app.chat.cursor += c.len_utf8();
        }
        _ => {}
    }
}

/// Find the byte offset of the start of the line containing `pos`.
fn line_start(s: &str, pos: usize) -> usize {
    s[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

/// Find the byte offset of the end of the line containing `pos` (before the '\n').
fn line_end(s: &str, pos: usize) -> usize {
    s[pos..].find('\n').map(|i| pos + i).unwrap_or(s.len())
}

/// Move cursor up one line, preserving column position as best as possible.
fn move_cursor_up(s: &str, pos: usize) -> usize {
    let cur_line_start = line_start(s, pos);
    if cur_line_start == 0 {
        // Already on the first line.
        return pos;
    }
    let col = s[cur_line_start..pos].chars().count();
    // Previous line ends at cur_line_start - 1 (the '\n').
    let prev_line_start = line_start(s, cur_line_start - 1);
    let prev_line_end = cur_line_start - 1; // byte offset of the '\n'
    let prev_line = &s[prev_line_start..prev_line_end];
    let target_col = col.min(prev_line.chars().count());
    // Convert char offset to byte offset.
    let byte_offset: usize = prev_line.chars().take(target_col).map(|c| c.len_utf8()).sum();
    prev_line_start + byte_offset
}

/// Move cursor down one line, preserving column position as best as possible.
fn move_cursor_down(s: &str, pos: usize) -> usize {
    let cur_line_end = line_end(s, pos);
    if cur_line_end >= s.len() {
        // Already on the last line.
        return pos;
    }
    let cur_line_start = line_start(s, pos);
    let col = s[cur_line_start..pos].chars().count();
    let next_line_start = cur_line_end + 1; // skip the '\n'
    let next_line_end = line_end(s, next_line_start);
    let next_line = &s[next_line_start..next_line_end];
    let target_col = col.min(next_line.chars().count());
    let byte_offset: usize = next_line.chars().take(target_col).map(|c| c.len_utf8()).sum();
    next_line_start + byte_offset
}

fn handle_message_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Enter => {
            let text = app.messages_panel.input.clone();
            app.messages_panel.input.clear();
            app.messages_panel.cursor = 0;
            if !text.trim().is_empty() {
                if let Some(task_id) = app.messages_panel.task_id.clone() {
                    app.exec_command(
                        vec![
                            "msg".to_string(),
                            "send".to_string(),
                            task_id.clone(),
                            text,
                            "--from".to_string(),
                            "tui".to_string(),
                        ],
                        CommandEffect::Notify(format!("Message sent to '{}'", task_id)),
                    );
                    // Invalidate so messages reload on next frame.
                    app.invalidate_messages_panel();
                    app.load_messages_panel();
                }
            }
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            if app.messages_panel.cursor > 0 {
                let prev = prev_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
                app.messages_panel.input.drain(prev..app.messages_panel.cursor);
                app.messages_panel.cursor = prev;
            }
        }
        KeyCode::Delete => {
            if app.messages_panel.cursor < app.messages_panel.input.len() {
                let next = next_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
                app.messages_panel.input.drain(app.messages_panel.cursor..next);
            }
        }
        // Ctrl+A: move to beginning
        KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.messages_panel.cursor = 0;
        }
        // Ctrl+E: move to end
        KeyCode::Char('e') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.messages_panel.cursor = app.messages_panel.input.len();
        }
        // Ctrl+B: move backward one char
        KeyCode::Char('b') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.messages_panel.cursor = prev_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
        }
        // Ctrl+F: move forward one char
        KeyCode::Char('f') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.messages_panel.cursor = next_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
        }
        // Ctrl+K: kill to end of line
        KeyCode::Char('k') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.messages_panel.input.truncate(app.messages_panel.cursor);
        }
        // Ctrl+U: kill to beginning of line
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.messages_panel.input.drain(..app.messages_panel.cursor);
            app.messages_panel.cursor = 0;
        }
        // Ctrl+W: delete word backward
        KeyCode::Char('w') if modifiers.contains(KeyModifiers::CONTROL) => {
            let start = word_boundary_back(&app.messages_panel.input, app.messages_panel.cursor);
            app.messages_panel.input.drain(start..app.messages_panel.cursor);
            app.messages_panel.cursor = start;
        }
        // Ctrl+D: delete char forward (or no-op at end)
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            if app.messages_panel.cursor < app.messages_panel.input.len() {
                let next = next_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
                app.messages_panel.input.drain(app.messages_panel.cursor..next);
            }
        }
        // Ctrl+C: cancel input
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.messages_panel.input.clear();
            app.messages_panel.cursor = 0;
            app.input_mode = InputMode::Normal;
        }
        // Arrow keys
        KeyCode::Left => {
            app.messages_panel.cursor = prev_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
        }
        KeyCode::Right => {
            app.messages_panel.cursor = next_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
        }
        KeyCode::Home => {
            app.messages_panel.cursor = 0;
        }
        KeyCode::End => {
            app.messages_panel.cursor = app.messages_panel.input.len();
        }
        // Scroll message history while typing
        KeyCode::Up if modifiers.contains(KeyModifiers::ALT) => {
            app.messages_panel.scroll = app.messages_panel.scroll.saturating_sub(1);
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::ALT) => {
            app.messages_panel.scroll += 1;
        }
        KeyCode::Char(c) => {
            app.messages_panel.input.insert(app.messages_panel.cursor, c);
            app.messages_panel.cursor += c.len_utf8();
        }
        _ => {}
    }
}

/// Find the byte offset of the previous char boundary (or 0).
fn prev_char_boundary(s: &str, pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    let mut i = pos - 1;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Find the byte offset of the next char boundary (or len).
fn next_char_boundary(s: &str, pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    let mut i = pos + 1;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Find the start of the previous word (skips whitespace, then non-whitespace).
fn word_boundary_back(s: &str, pos: usize) -> usize {
    let bytes = s.as_bytes();
    let mut i = pos;
    // Skip whitespace backward
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    // Skip non-whitespace backward
    while i > 0 && !bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    i
}

fn handle_normal_key(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    match app.focused_panel {
        FocusedPanel::Graph => handle_graph_key(app, code, modifiers),
        FocusedPanel::RightPanel => handle_right_panel_key(app, code, modifiers),
    }
}

fn handle_graph_key(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        // Help overlay
        KeyCode::Char('?') => app.show_help = true,

        // Quit
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Esc => {
            if app.has_active_search() {
                app.clear_search();
            } else {
                app.should_quit = true;
            }
        }
        // Ctrl+C: kill the agent on the focused task (not quit — use `q` to quit)
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.kill_focused_agent();
        }

        // Search
        KeyCode::Char('/') => {
            app.search_active = true;
            app.input_mode = InputMode::Search;
            app.search_input.clear();
            app.fuzzy_matches.clear();
            app.current_match = None;
            app.filtered_indices = None;
            app.update_scroll_bounds();
        }

        // Tab: switch panel focus (replaces old trace toggle)
        KeyCode::Tab => {
            app.toggle_panel_focus();
        }

        // t: toggle trace (was Tab)
        KeyCode::Char('t') => {
            app.toggle_trace();
        }

        // T: toggle token display (was t)
        KeyCode::Char('T') => {
            app.show_total_tokens = !app.show_total_tokens;
        }

        // Backslash: toggle right panel visibility
        KeyCode::Char('\\') => {
            app.toggle_right_panel();
        }

        // Cycle HUD panel size (1/3 ↔ 2/3)
        KeyCode::Char('=') => {
            app.cycle_hud_size();
        }

        // Navigate between matches
        KeyCode::Char('n') => app.next_match(),
        KeyCode::Char('N') | KeyCode::BackTab => app.prev_match(),

        // HUD panel scroll (Shift or Alt + Up/Down/PgUp/PgDn)
        KeyCode::Up
            if modifiers.contains(KeyModifiers::SHIFT)
                || modifiers.contains(KeyModifiers::ALT) =>
        {
            app.hud_scroll_up(1);
        }
        KeyCode::Down
            if modifiers.contains(KeyModifiers::SHIFT)
                || modifiers.contains(KeyModifiers::ALT) =>
        {
            app.hud_scroll_down(1);
        }
        KeyCode::PageUp
            if modifiers.contains(KeyModifiers::SHIFT)
                || modifiers.contains(KeyModifiers::ALT) =>
        {
            app.hud_scroll_up(10);
        }
        KeyCode::PageDown
            if modifiers.contains(KeyModifiers::SHIFT)
                || modifiers.contains(KeyModifiers::ALT) =>
        {
            app.hud_scroll_down(10);
        }

        // Arrow keys: navigate tasks when trace is visible, scroll viewport when off
        KeyCode::Up => {
            if app.trace_visible {
                app.select_prev_task();
            } else {
                app.scroll.scroll_up(1);
            }
        }
        KeyCode::Down => {
            if app.trace_visible {
                app.select_next_task();
            } else {
                app.scroll.scroll_down(1);
            }
        }

        // Vertical scroll (vim-style)
        KeyCode::Char('k') => app.scroll.scroll_up(1),
        KeyCode::Char('j') => app.scroll.scroll_down(1),
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => app.scroll.page_up(),
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => app.scroll.page_down(),
        KeyCode::PageUp => app.scroll.page_up(),
        KeyCode::PageDown => app.scroll.page_down(),

        // Jump to top/bottom
        KeyCode::Char('g') => app.scroll.go_top(),
        KeyCode::Char('G') => app.scroll.go_bottom(),
        KeyCode::Home => {
            app.scroll.go_top();
            app.select_first_task();
        }
        KeyCode::End => {
            app.scroll.go_bottom();
            app.select_last_task();
        }

        // Sort mode cycle
        KeyCode::Char('s') => app.cycle_sort_mode(),

        // Manual refresh
        KeyCode::Char('r') => app.force_refresh(),

        // Toggle mouse capture
        KeyCode::Char('m') => {
            app.toggle_mouse();
            let _ = set_mouse_capture(app.mouse_enabled);
        }

        // Toggle log pane
        KeyCode::Char('L') => app.toggle_log_pane(),

        // Toggle log pane JSON mode
        KeyCode::Char('J') => app.toggle_log_json(),

        // Horizontal scroll
        KeyCode::Left | KeyCode::Char('h') => app.scroll.scroll_left(4),
        KeyCode::Right | KeyCode::Char('l') => app.scroll.scroll_right(4),

        // ── Quick action keys (require a selected task) ──

        // a: open task creation form
        KeyCode::Char('a') => {
            app.open_task_form();
        }

        // d: mark selected task done (confirm dialog)
        KeyCode::Char('D') => {
            if let Some(task_id) = app.selected_task_id().map(|s| s.to_string()) {
                app.input_mode = InputMode::Confirm(ConfirmAction::MarkDone(task_id));
            }
        }

        // f: mark selected task failed (text prompt for reason)
        KeyCode::Char('f') => {
            if let Some(task_id) = app.selected_task_id().map(|s| s.to_string()) {
                app.text_prompt.input.clear();
                app.input_mode =
                    InputMode::TextPrompt(TextPromptAction::MarkFailed(task_id));
            }
        }

        // x: retry selected task (confirm dialog)
        KeyCode::Char('x') => {
            if let Some(task_id) = app.selected_task_id().map(|s| s.to_string()) {
                app.input_mode = InputMode::Confirm(ConfirmAction::Retry(task_id));
            }
        }

        // e: edit task description (text prompt)
        KeyCode::Char('e') => {
            if let Some(task_id) = app.selected_task_id().map(|s| s.to_string()) {
                app.text_prompt.input.clear();
                app.input_mode =
                    InputMode::TextPrompt(TextPromptAction::EditDescription(task_id));
            }
        }

        // c or ':': open chat input (switch to chat tab + enter input mode)
        // Preserves any in-progress input from previous editing.
        KeyCode::Char('c') | KeyCode::Char(':') => {
            app.right_panel_visible = true;
            app.right_panel_tab = RightPanelTab::Chat;
            app.focused_panel = FocusedPanel::RightPanel;
            app.input_mode = InputMode::ChatInput;
        }

        // Digit keys 0-4: switch right panel tab
        KeyCode::Char(d @ '0'..='4') => {
            let idx = (d as u8 - b'0') as usize;
            if let Some(tab) = RightPanelTab::from_index(idx) {
                app.right_panel_visible = true;
                app.right_panel_tab = tab;
            }
        }

        _ => {}
    }
}

fn handle_right_panel_key(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        // Global keys that work in right panel too
        KeyCode::Char('?') => app.show_help = true,
        KeyCode::Char('q') => app.should_quit = true,
        // Ctrl+C: kill the agent on the focused task (same as graph panel)
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.kill_focused_agent();
        }

        // Tab: switch panel focus back to graph
        KeyCode::Tab => {
            app.toggle_panel_focus();
        }

        // Backslash: toggle right panel
        KeyCode::Char('\\') => {
            app.toggle_right_panel();
        }

        // Cycle HUD panel size (1/3 ↔ 2/3)
        KeyCode::Char('=') => {
            app.cycle_hud_size();
        }

        // Esc: go back to graph focus
        KeyCode::Esc => {
            app.focused_panel = FocusedPanel::Graph;
        }

        // Number keys 0-4 switch tabs
        KeyCode::Char(d @ '0'..='4') => {
            let idx = (d as u8 - b'0') as usize;
            if let Some(tab) = RightPanelTab::from_index(idx) {
                app.right_panel_tab = tab;
            }
        }

        // Left/Right cycle tabs
        KeyCode::Left => app.right_panel_tab = app.right_panel_tab.prev(),
        KeyCode::Right => app.right_panel_tab = app.right_panel_tab.next(),

        // Up/Down/k/j scroll the active panel content
        KeyCode::Up | KeyCode::Char('k') => {
            right_panel_scroll_up(app, 1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            right_panel_scroll_down(app, 1);
        }

        // PgUp/PgDn fast scroll
        KeyCode::PageUp => {
            right_panel_scroll_up(app, 10);
        }
        KeyCode::PageDown => {
            right_panel_scroll_down(app, 10);
        }

        // Enter: in chat tab, enter chat input mode; in messages tab, enter message input mode
        // Preserves any in-progress chat input from previous editing.
        KeyCode::Enter => {
            if app.right_panel_tab == RightPanelTab::Chat {
                app.input_mode = InputMode::ChatInput;
            } else if app.right_panel_tab == RightPanelTab::Messages {
                app.messages_panel.input.clear();
                app.messages_panel.cursor = 0;
                app.input_mode = InputMode::MessageInput;
            }
        }

        _ => {}
    }
}

fn right_panel_scroll_up(app: &mut VizApp, amount: usize) {
    match app.right_panel_tab {
        RightPanelTab::Detail => app.hud_scroll_up(amount),
        RightPanelTab::Chat => {
            app.chat.scroll += amount;
        }
        RightPanelTab::Log => {
            app.log_scroll_up(amount);
        }
        RightPanelTab::Messages => {
            app.messages_panel.scroll = app.messages_panel.scroll.saturating_sub(amount);
        }
        RightPanelTab::Agency => {
            app.agent_monitor.scroll = app.agent_monitor.scroll.saturating_sub(amount);
        }
    }
}

fn right_panel_scroll_down(app: &mut VizApp, amount: usize) {
    match app.right_panel_tab {
        RightPanelTab::Detail => {
            app.hud_scroll_down(amount);
        }
        RightPanelTab::Chat => {
            app.chat.scroll = app.chat.scroll.saturating_sub(amount);
        }
        RightPanelTab::Log => {
            app.log_scroll_down(amount);
        }
        RightPanelTab::Messages => {
            app.messages_panel.scroll += amount;
        }
        RightPanelTab::Agency => {
            app.agent_monitor.scroll += amount;
        }
    }
}


fn handle_mouse(app: &mut VizApp, kind: MouseEventKind, row: u16, column: u16) {
    let pos = Position::new(column, row);
    let in_graph = app.last_graph_area.contains(pos);
    let in_tab_bar = app.last_tab_bar_area.contains(pos);
    let in_right_content = app.last_right_content_area.contains(pos);

    match kind {
        MouseEventKind::ScrollUp => {
            if in_graph {
                app.scroll.scroll_up(3);
            } else if in_right_content || in_tab_bar {
                right_panel_scroll_up(app, 3);
            } else {
                app.scroll.scroll_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_graph {
                app.scroll.scroll_down(3);
            } else if in_right_content || in_tab_bar {
                right_panel_scroll_down(app, 3);
            } else {
                app.scroll.scroll_down(3);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if in_tab_bar {
                // Click on tab header: switch to that tab.
                let col_in_tabs = column.saturating_sub(app.last_tab_bar_area.x);
                if let Some(tab) = tab_at_column(col_in_tabs) {
                    app.right_panel_tab = tab;
                    app.focused_panel = FocusedPanel::RightPanel;
                }
            } else if in_right_content {
                // Click in right panel content: focus the right panel.
                app.focused_panel = FocusedPanel::RightPanel;
            } else if in_graph {
                // Click in graph: focus graph + select task at clicked line.
                app.focused_panel = FocusedPanel::Graph;
                let content_row = row.saturating_sub(app.last_graph_area.y);
                let visible_idx = app.scroll.offset_y + content_row as usize;
                if visible_idx < app.visible_line_count() {
                    let orig_line = app.visible_to_original(visible_idx);
                    app.select_task_at_line(orig_line);
                }
            }
        }
        _ => {}
    }
}

/// Determine which tab was clicked based on column position within the tab bar.
/// Returns None if the click is on a divider or beyond the last tab.
fn tab_at_column(col: u16) -> Option<RightPanelTab> {
    let labels = ["0:Chat", "1:Detail", "2:Log", "3:Msg", "4:Agency"];
    let mut pos: u16 = 0;
    for (i, label) in labels.iter().enumerate() {
        if i > 0 {
            pos += 1; // divider "│" is 1 column wide
        }
        let tab_width = label.len() as u16 + 2; // " label " padding
        if col >= pos && col < pos + tab_width {
            return RightPanelTab::from_index(i);
        }
        pos += tab_width;
    }
    None
}
