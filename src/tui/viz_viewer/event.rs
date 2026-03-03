use std::io;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::DefaultTerminal;
use ratatui::layout::Position;

use super::render;
use super::state::{
    CommandEffect, ConfigEditKind, ConfirmAction, FocusedPanel, InputMode, RightPanelTab,
    TaskFormField, TextPromptAction, VizApp,
};

/// Input poll timeout — short for responsive scrolling.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// Apply the current mouse capture state to the terminal.
///
/// Uses modes 1002 (button-event tracking) and 1006 (SGR extended coordinates)
/// instead of crossterm's EnableMouseCapture which also enables 1003 (any-event).
/// Mode 1003 breaks mosh compatibility because mosh disables earlier modes when
/// a new mode arrives, and Termux doesn't support 1003 — leaving no tracking
/// mode active. Mode 1002 adds drag reporting (motion while button held) on top
/// of 1000 (button tracking), which is needed for scrollbar dragging.
fn set_mouse_capture(enabled: bool) -> Result<()> {
    use io::Write;
    let mut stdout = io::stdout();
    if enabled {
        stdout.write_all(b"\x1b[?1002h\x1b[?1006h")?;
    } else {
        stdout.write_all(b"\x1b[?1006l\x1b[?1002l")?;
    }
    stdout.flush()?;
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
    // Read crossterm events in a background thread and feed them through a
    // channel.  This prevents event::read() from blocking the main loop when
    // the terminal layers (e.g. mosh) deliver a bracketed-paste slowly —
    // crossterm blocks until the closing ESC[201~ arrives, and in a
    // Termux → Mosh → Tmux → TUI chain that can stall for seconds.
    let (tx, rx) = mpsc::sync_channel::<Event>(512);
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if tx.send(ev).is_err() {
                break; // receiver dropped — main loop exited
            }
        }
    });

    loop {
        app.maybe_refresh();
        app.drain_commands();
        terminal.draw(|frame| render::draw(frame, app))?;

        // Wait for the first event (up to INPUT_POLL), then drain all
        // immediately queued events before redrawing — same batching
        // strategy as before, but via the channel instead of raw polling.
        match rx.recv_timeout(INPUT_POLL) {
            Ok(ev) => {
                dispatch_event(app, ev);
                // Drain remaining queued events so we only redraw once
                // for a rapid burst (e.g. pasted text arriving as
                // individual KeyEvents when bracketed paste is absent).
                while let Ok(ev) = rx.try_recv() {
                    dispatch_event(app, ev);
                    if app.should_quit {
                        break;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {} // no events — just redraw
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("terminal event reader thread exited unexpectedly");
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

/// Route a single crossterm event to the appropriate handler.
fn dispatch_event(app: &mut VizApp, ev: Event) {
    match ev {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            handle_key(app, key.code, key.modifiers);
        }
        Event::Paste(text) => {
            handle_paste(app, &text);
        }
        Event::Mouse(mouse) if app.mouse_enabled => {
            handle_mouse(app, mouse.kind, mouse.row, mouse.column);
        }
        Event::Resize(_, _) => {} // handled by next redraw
        _ => {}
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
        InputMode::ConfigEdit => handle_config_edit_input(app, code, modifiers),
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
        InputMode::MessageInput => {
            // Strip newlines for message input — it's single-line.
            let clean: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            app.messages_panel
                .input
                .insert_str(app.messages_panel.cursor, &clean);
            app.messages_panel.cursor += clean.len();
        }
        InputMode::ConfigEdit => {
            let clean: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            app.config_panel.edit_buffer.push_str(&clean);
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
        KeyCode::Left => {
            app.record_graph_scroll_activity();
            app.record_graph_hscroll_activity();
            app.scroll.scroll_left(4);
        }
        KeyCode::Right => {
            app.record_graph_scroll_activity();
            app.record_graph_hscroll_activity();
            app.scroll.scroll_right(4);
        }
        KeyCode::Up => {
            app.record_graph_scroll_activity();
            app.scroll.scroll_up(1);
        }
        KeyCode::Down => {
            app.record_graph_scroll_activity();
            app.scroll.scroll_down(1);
        }
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
            // Return to ChatInput mode if cancelling an attach prompt.
            if action == TextPromptAction::AttachFile {
                app.input_mode = InputMode::ChatInput;
            } else {
                app.input_mode = InputMode::Normal;
            }
        }
        KeyCode::Enter => {
            let text = app.text_prompt.input.clone();
            app.text_prompt.input.clear();
            if text.trim().is_empty() {
                if action == TextPromptAction::AttachFile {
                    app.input_mode = InputMode::ChatInput;
                } else {
                    app.input_mode = InputMode::Normal;
                }
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
                        vec!["edit".to_string(), task_id.clone(), "-d".to_string(), text],
                        CommandEffect::RefreshAndNotify(format!("Updated '{}'", task_id)),
                    );
                }
                TextPromptAction::AttachFile => {
                    app.attach_file(&text);
                    // Return to chat input mode, not normal mode.
                    app.input_mode = InputMode::ChatInput;
                    return;
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
                if !form.dep_matches.is_empty() && form.dep_match_idx < form.dep_matches.len() - 1 {
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
            app.chat_input_dismissed = true;
        }
        KeyCode::Enter => {
            // Enter sends the message (newlines from paste are preserved in the content).
            // Stay in ChatInput mode so the user can immediately type another message.
            let text = app.chat.input.clone();
            app.chat.input.clear();
            app.chat.cursor = 0;
            app.chat.input_scroll = 0;
            if !text.trim().is_empty() {
                app.send_chat_message(text);
            }
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
        // Ctrl+A: attach file
        KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.text_prompt.input.clear();
            app.input_mode = InputMode::TextPrompt(TextPromptAction::AttachFile);
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
        // Ctrl+V: check clipboard for image before falling through to text paste.
        // In terminals with bracketed paste, Ctrl+V triggers Event::Paste for text.
        // This handler catches the KeyEvent variant to probe for image data first.
        KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.try_paste_clipboard_image();
            // If no image was found, text paste will arrive via Event::Paste — nothing more to do.
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
            app.record_panel_scroll_activity();
            app.chat.scroll = app.chat.scroll.saturating_add(1);
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::ALT) => {
            app.record_panel_scroll_activity();
            app.chat.scroll = app.chat.scroll.saturating_sub(1);
        }
        // Up/Down: navigate between lines in multi-line input
        KeyCode::Up => {
            let new_pos = move_cursor_up(&app.chat.input, app.chat.cursor);
            if new_pos == app.chat.cursor {
                // Already on first line: scroll chat history instead.
                app.record_panel_scroll_activity();
                app.chat.scroll = app.chat.scroll.saturating_add(1);
            } else {
                app.chat.cursor = new_pos;
            }
        }
        KeyCode::Down => {
            let new_pos = move_cursor_down(&app.chat.input, app.chat.cursor);
            if new_pos == app.chat.cursor {
                // Already on last line: scroll chat history instead.
                app.record_panel_scroll_activity();
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
    let byte_offset: usize = prev_line
        .chars()
        .take(target_col)
        .map(|c| c.len_utf8())
        .sum();
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
    let byte_offset: usize = next_line
        .chars()
        .take(target_col)
        .map(|c| c.len_utf8())
        .sum();
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
            if !text.trim().is_empty()
                && let Some(task_id) = app.messages_panel.task_id.clone()
            {
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
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            if app.messages_panel.cursor > 0 {
                let prev = prev_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
                app.messages_panel
                    .input
                    .drain(prev..app.messages_panel.cursor);
                app.messages_panel.cursor = prev;
            }
        }
        KeyCode::Delete => {
            if app.messages_panel.cursor < app.messages_panel.input.len() {
                let next = next_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
                app.messages_panel
                    .input
                    .drain(app.messages_panel.cursor..next);
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
            app.messages_panel.cursor =
                prev_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
        }
        // Ctrl+F: move forward one char
        KeyCode::Char('f') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.messages_panel.cursor =
                next_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
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
            app.messages_panel
                .input
                .drain(start..app.messages_panel.cursor);
            app.messages_panel.cursor = start;
        }
        // Ctrl+D: delete char forward (or no-op at end)
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            if app.messages_panel.cursor < app.messages_panel.input.len() {
                let next = next_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
                app.messages_panel
                    .input
                    .drain(app.messages_panel.cursor..next);
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
            app.messages_panel.cursor =
                prev_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
        }
        KeyCode::Right => {
            app.messages_panel.cursor =
                next_char_boundary(&app.messages_panel.input, app.messages_panel.cursor);
        }
        KeyCode::Home => {
            app.messages_panel.cursor = 0;
        }
        KeyCode::End => {
            app.messages_panel.cursor = app.messages_panel.input.len();
        }
        // Scroll message history while typing
        KeyCode::Up if modifiers.contains(KeyModifiers::ALT) => {
            app.record_panel_scroll_activity();
            app.messages_panel.scroll = app.messages_panel.scroll.saturating_sub(1);
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::ALT) => {
            app.record_panel_scroll_activity();
            app.messages_panel.scroll += 1;
        }
        KeyCode::Char(c) => {
            app.messages_panel
                .input
                .insert(app.messages_panel.cursor, c);
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
            // Auto-enter ChatInput when focusing right panel on Chat tab,
            // but only if user hasn't explicitly dismissed it with Esc.
            if app.focused_panel == FocusedPanel::RightPanel
                && app.right_panel_tab == RightPanelTab::Chat
                && !app.chat_input_dismissed
            {
                app.input_mode = InputMode::ChatInput;
            }
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

        // Cycle layout mode: split → full panel → full graph
        KeyCode::Char('=') => {
            app.cycle_layout_mode();
        }

        // Navigate between matches
        KeyCode::Char('n') => app.next_match(),
        KeyCode::Char('N') | KeyCode::BackTab => app.prev_match(),

        // Alt+Up/Down: toggle focus between graph and right panel
        KeyCode::Up if modifiers.contains(KeyModifiers::ALT) => {
            app.toggle_panel_focus();
            if app.focused_panel == FocusedPanel::RightPanel
                && app.right_panel_tab == RightPanelTab::Chat
                && !app.chat_input_dismissed
            {
                app.input_mode = InputMode::ChatInput;
            }
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::ALT) => {
            app.toggle_panel_focus();
            if app.focused_panel == FocusedPanel::RightPanel
                && app.right_panel_tab == RightPanelTab::Chat
                && !app.chat_input_dismissed
            {
                app.input_mode = InputMode::ChatInput;
            }
        }

        // Alt+Left/Right: cycle tabs
        KeyCode::Left if modifiers.contains(KeyModifiers::ALT) => {
            let old_tab = app.right_panel_tab;
            app.right_panel_visible = true;
            app.right_panel_tab = app.right_panel_tab.prev();
            if old_tab == RightPanelTab::Chat && app.right_panel_tab != RightPanelTab::Chat {
                app.chat_input_dismissed = false;
            }
        }
        KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => {
            let old_tab = app.right_panel_tab;
            app.right_panel_visible = true;
            app.right_panel_tab = app.right_panel_tab.next();
            if old_tab == RightPanelTab::Chat && app.right_panel_tab != RightPanelTab::Chat {
                app.chat_input_dismissed = false;
            }
        }

        // HUD panel scroll (Shift + Up/Down/PgUp/PgDn)
        KeyCode::Up if modifiers.contains(KeyModifiers::SHIFT) => {
            app.record_panel_scroll_activity();
            app.hud_scroll_up(1);
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::SHIFT) => {
            app.record_panel_scroll_activity();
            app.hud_scroll_down(1);
        }
        KeyCode::PageUp if modifiers.contains(KeyModifiers::SHIFT) => {
            app.record_panel_scroll_activity();
            app.hud_scroll_up(10);
        }
        KeyCode::PageDown if modifiers.contains(KeyModifiers::SHIFT) => {
            app.record_panel_scroll_activity();
            app.hud_scroll_down(10);
        }

        // Arrow keys: always navigate tasks in graph view
        KeyCode::Up => {
            app.select_prev_task();
        }
        KeyCode::Down => {
            app.select_next_task();
        }

        // Vertical scroll (vim-style)
        KeyCode::Char('k') => {
            app.record_graph_scroll_activity();
            app.scroll.scroll_up(1);
        }
        KeyCode::Char('j') => {
            app.record_graph_scroll_activity();
            app.scroll.scroll_down(1);
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.record_graph_scroll_activity();
            app.scroll.page_up();
        }
        KeyCode::Char('d') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.record_graph_scroll_activity();
            app.scroll.page_down();
        }
        KeyCode::PageUp => {
            // Jump by half a screenful of tasks
            let jump = (app.scroll.viewport_height / 2).max(1);
            app.select_prev_task_n(jump);
        }
        KeyCode::PageDown => {
            let jump = (app.scroll.viewport_height / 2).max(1);
            app.select_next_task_n(jump);
        }

        // Jump to top/bottom
        KeyCode::Char('g') => {
            app.record_graph_scroll_activity();
            app.scroll.go_top();
        }
        KeyCode::Char('G') => {
            app.record_graph_scroll_activity();
            app.scroll.go_bottom();
        }
        KeyCode::Home => {
            app.record_graph_scroll_activity();
            app.scroll.go_top();
            app.select_first_task();
        }
        KeyCode::End => {
            app.record_graph_scroll_activity();
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

        // Toggle coordinator log view
        KeyCode::Char('L') => app.toggle_coord_log(),

        // Toggle log pane JSON mode
        KeyCode::Char('J') => app.toggle_log_json(),

        // Horizontal scroll
        KeyCode::Left | KeyCode::Char('h')
            if !modifiers.contains(KeyModifiers::SHIFT)
                && !modifiers.contains(KeyModifiers::ALT) =>
        {
            app.record_graph_hscroll_activity();
            app.scroll.scroll_left(4);
        }
        KeyCode::Right | KeyCode::Char('l')
            if !modifiers.contains(KeyModifiers::SHIFT)
                && !modifiers.contains(KeyModifiers::ALT) =>
        {
            app.record_graph_hscroll_activity();
            app.scroll.scroll_right(4);
        }
        KeyCode::Left if modifiers.contains(KeyModifiers::SHIFT) => {
            app.record_graph_hscroll_activity();
            app.scroll.page_left();
        }
        KeyCode::Right if modifiers.contains(KeyModifiers::SHIFT) => {
            app.record_graph_hscroll_activity();
            app.scroll.page_right();
        }

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
                app.input_mode = InputMode::TextPrompt(TextPromptAction::MarkFailed(task_id));
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
                app.input_mode = InputMode::TextPrompt(TextPromptAction::EditDescription(task_id));
            }
        }

        // c or ':': open chat input (switch to chat tab + enter input mode)
        // Preserves any in-progress input from previous editing.
        KeyCode::Char('c') | KeyCode::Char(':') => {
            app.right_panel_visible = true;
            app.right_panel_tab = RightPanelTab::Chat;
            app.focused_panel = FocusedPanel::RightPanel;
            app.chat_input_dismissed = false;
            app.input_mode = InputMode::ChatInput;
        }

        // Digit keys 0-6: switch right panel tab
        KeyCode::Char(d @ '0'..='7') => {
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
    // Files tab has its own key handler — intercept early.
    // When search mode is active, only Ctrl+C stays global; everything else
    // (including Esc, character keys) goes to the file browser handler.
    if app.right_panel_tab == RightPanelTab::Files {
        let is_searching = app.file_browser.as_ref().is_some_and(|fb| fb.searching);
        if is_searching {
            match code {
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.kill_focused_agent();
                }
                _ => handle_files_key(app, code),
            }
        } else {
            match code {
                KeyCode::Char('?') => app.show_help = true,
                KeyCode::Char('q') => app.should_quit = true,
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    app.kill_focused_agent();
                }
                KeyCode::Char('\\') => app.toggle_right_panel(),
                KeyCode::Char('=') => app.cycle_layout_mode(),
                KeyCode::Esc => {
                    app.focused_panel = FocusedPanel::Graph;
                    app.chat_input_dismissed = false;
                }
                KeyCode::Char(d @ '0'..='7') => {
                    let idx = (d as u8 - b'0') as usize;
                    if let Some(tab) = RightPanelTab::from_index(idx) {
                        app.right_panel_tab = tab;
                        if tab == RightPanelTab::Chat && !app.chat_input_dismissed {
                            app.input_mode = InputMode::ChatInput;
                        }
                    }
                }
                _ => handle_files_key(app, code),
            }
        }
        return;
    }

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
            // Reset dismissed flag when leaving right panel
            if app.focused_panel == FocusedPanel::Graph {
                app.chat_input_dismissed = false;
            }
        }

        // Backslash: toggle right panel
        KeyCode::Char('\\') => {
            app.toggle_right_panel();
        }

        // Cycle layout mode: split → full panel → full graph
        KeyCode::Char('=') => {
            app.cycle_layout_mode();
        }

        // Esc: go back to graph focus
        KeyCode::Esc => {
            app.focused_panel = FocusedPanel::Graph;
            // Reset dismissed flag so auto-enter works next time user returns
            app.chat_input_dismissed = false;
        }

        // Number keys 0-6 switch tabs
        KeyCode::Char(d @ '0'..='7') => {
            let idx = (d as u8 - b'0') as usize;
            if let Some(tab) = RightPanelTab::from_index(idx) {
                // Reset dismissed flag when navigating away from Chat tab
                if app.right_panel_tab == RightPanelTab::Chat && tab != RightPanelTab::Chat {
                    app.chat_input_dismissed = false;
                }
                app.right_panel_tab = tab;
                // Auto-enter ChatInput when switching to Chat tab,
                // unless user explicitly dismissed with Esc.
                if tab == RightPanelTab::Chat && !app.chat_input_dismissed {
                    app.input_mode = InputMode::ChatInput;
                }
            }
        }

        // Alt+Up/Down: toggle panel focus
        KeyCode::Up if modifiers.contains(KeyModifiers::ALT) => {
            app.toggle_panel_focus();
            if app.focused_panel == FocusedPanel::Graph {
                app.chat_input_dismissed = false;
            }
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::ALT) => {
            app.toggle_panel_focus();
            if app.focused_panel == FocusedPanel::Graph {
                app.chat_input_dismissed = false;
            }
        }

        // Alt+Left/Right: cycle tabs (same as bare Left/Right)
        KeyCode::Left if modifiers.contains(KeyModifiers::ALT) => {
            let old_tab = app.right_panel_tab;
            app.right_panel_tab = app.right_panel_tab.prev();
            if old_tab == RightPanelTab::Chat && app.right_panel_tab != RightPanelTab::Chat {
                app.chat_input_dismissed = false;
            }
            if app.right_panel_tab == RightPanelTab::Chat && !app.chat_input_dismissed {
                app.input_mode = InputMode::ChatInput;
            }
        }
        KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => {
            let old_tab = app.right_panel_tab;
            app.right_panel_tab = app.right_panel_tab.next();
            if old_tab == RightPanelTab::Chat && app.right_panel_tab != RightPanelTab::Chat {
                app.chat_input_dismissed = false;
            }
            if app.right_panel_tab == RightPanelTab::Chat && !app.chat_input_dismissed {
                app.input_mode = InputMode::ChatInput;
            }
        }

        // Left/Right cycle tabs
        KeyCode::Left => {
            let old_tab = app.right_panel_tab;
            app.right_panel_tab = app.right_panel_tab.prev();
            if old_tab == RightPanelTab::Chat && app.right_panel_tab != RightPanelTab::Chat {
                app.chat_input_dismissed = false;
            }
            if app.right_panel_tab == RightPanelTab::Chat && !app.chat_input_dismissed {
                app.input_mode = InputMode::ChatInput;
            }
        }
        KeyCode::Right => {
            let old_tab = app.right_panel_tab;
            app.right_panel_tab = app.right_panel_tab.next();
            if old_tab == RightPanelTab::Chat && app.right_panel_tab != RightPanelTab::Chat {
                app.chat_input_dismissed = false;
            }
            if app.right_panel_tab == RightPanelTab::Chat && !app.chat_input_dismissed {
                app.input_mode = InputMode::ChatInput;
            }
        }

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

        // Home/End: jump to top/bottom of content
        KeyCode::Home => {
            right_panel_scroll_to_top(app);
        }
        KeyCode::End => {
            right_panel_scroll_to_bottom(app);
        }

        // Enter: in chat tab, enter chat input mode; in messages tab, enter message input mode;
        // in config tab, start editing the selected setting.
        KeyCode::Enter => {
            if app.right_panel_tab == RightPanelTab::Chat {
                app.chat_input_dismissed = false;
                app.input_mode = InputMode::ChatInput;
            } else if app.right_panel_tab == RightPanelTab::Messages {
                app.messages_panel.input.clear();
                app.messages_panel.cursor = 0;
                app.input_mode = InputMode::MessageInput;
            } else if app.right_panel_tab == RightPanelTab::Config {
                config_enter_edit(app);
            }
        }

        // Config tab: Space toggles boolean entries
        KeyCode::Char(' ') if app.right_panel_tab == RightPanelTab::Config => {
            let idx = app.config_panel.selected;
            if idx < app.config_panel.entries.len() {
                if matches!(
                    app.config_panel.entries[idx].edit_kind,
                    ConfigEditKind::Toggle
                ) {
                    app.toggle_config_entry();
                } else {
                    config_enter_edit(app);
                }
            }
        }

        // Config tab: 'r' reloads config from disk
        KeyCode::Char('r') if app.right_panel_tab == RightPanelTab::Config => {
            app.load_config_panel();
        }

        // Config tab: 'a' starts the add-endpoint flow
        KeyCode::Char('a') if app.right_panel_tab == RightPanelTab::Config => {
            app.config_panel.adding_endpoint = true;
            app.config_panel.new_endpoint = super::state::NewEndpointFields::default();
            app.config_panel.new_endpoint_field = 0;
            app.config_panel.editing = false;
            app.input_mode = InputMode::ConfigEdit;
        }

        // Agency tab: 'a' = view assignment task detail, 'e' = view evaluation task detail
        KeyCode::Char('a') if app.right_panel_tab == RightPanelTab::Agency => {
            if let Some(ref lifecycle) = app.agency_lifecycle
                && let Some(ref phase) = lifecycle.assignment
            {
                let task_id = phase.task_id.clone();
                app.load_hud_detail_for_task(&task_id);
                app.right_panel_tab = RightPanelTab::Detail;
            }
        }
        KeyCode::Char('e') if app.right_panel_tab == RightPanelTab::Agency => {
            if let Some(ref lifecycle) = app.agency_lifecycle
                && let Some(ref phase) = lifecycle.evaluation
            {
                let task_id = phase.task_id.clone();
                app.load_hud_detail_for_task(&task_id);
                app.right_panel_tab = RightPanelTab::Detail;
            }
        }

        // Detail tab: 'R' toggles raw JSON display
        KeyCode::Char('R') if app.right_panel_tab == RightPanelTab::Detail => {
            app.detail_raw_json = !app.detail_raw_json;
            app.hud_detail = None; // force reload with new format
            app.load_hud_detail();
        }

        _ => {}
    }
}

fn right_panel_scroll_up(app: &mut VizApp, amount: usize) {
    app.record_panel_scroll_activity();
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
        RightPanelTab::Config => {
            // Skip collapsed entries when navigating up
            let visible = app.visible_config_entries();
            if let Some(pos) = visible
                .iter()
                .rposition(|(orig_idx, _)| *orig_idx < app.config_panel.selected)
            {
                app.config_panel.selected = visible[pos.saturating_sub(amount.saturating_sub(1))].0;
            }
        }
        RightPanelTab::Files => {
            // File browser handles its own scrolling.
        }
        RightPanelTab::CoordLog => {
            app.coord_log_scroll_up(amount);
        }
    }
}

fn right_panel_scroll_down(app: &mut VizApp, amount: usize) {
    app.record_panel_scroll_activity();
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
        RightPanelTab::Config => {
            // Skip collapsed entries when navigating down
            let visible = app.visible_config_entries();
            if let Some(pos) = visible
                .iter()
                .position(|(orig_idx, _)| *orig_idx > app.config_panel.selected)
            {
                let target = (pos + amount - 1).min(visible.len().saturating_sub(1));
                app.config_panel.selected = visible[target].0;
            }
        }
        RightPanelTab::Files => {
            // File browser handles its own scrolling.
        }
        RightPanelTab::CoordLog => {
            app.coord_log_scroll_down(amount);
        }
    }
}

fn right_panel_scroll_to_top(app: &mut VizApp) {
    app.record_panel_scroll_activity();
    match app.right_panel_tab {
        RightPanelTab::Detail => {
            app.hud_scroll = 0;
        }
        RightPanelTab::Chat => {
            // Chat scroll is from bottom (0 = fully scrolled down), so "top" = max.
            app.chat.scroll = usize::MAX;
        }
        RightPanelTab::Log => {
            app.log_scroll_to_top();
        }
        RightPanelTab::Messages => {
            app.messages_panel.scroll = 0;
        }
        RightPanelTab::Agency => {
            app.agent_monitor.scroll = 0;
        }
        RightPanelTab::Config => {
            let visible = app.visible_config_entries();
            if let Some(&(first, _)) = visible.first() {
                app.config_panel.selected = first;
            }
        }
        RightPanelTab::Files => {}
        RightPanelTab::CoordLog => {
            app.coord_log_scroll_to_top();
        }
    }
}

fn right_panel_scroll_to_bottom(app: &mut VizApp) {
    app.record_panel_scroll_activity();
    match app.right_panel_tab {
        RightPanelTab::Detail => {
            app.hud_scroll_down(usize::MAX);
        }
        RightPanelTab::Chat => {
            // Chat scroll is from bottom (0 = fully scrolled down), so "bottom" = 0.
            app.chat.scroll = 0;
        }
        RightPanelTab::Log => {
            app.log_scroll_to_bottom();
        }
        RightPanelTab::Messages => {
            app.messages_panel.scroll = usize::MAX;
        }
        RightPanelTab::Agency => {
            app.agent_monitor.scroll = usize::MAX;
        }
        RightPanelTab::Config => {
            let visible = app.visible_config_entries();
            if let Some(&(last, _)) = visible.last() {
                app.config_panel.selected = last;
            }
        }
        RightPanelTab::Files => {}
        RightPanelTab::CoordLog => {
            app.coord_log_scroll_to_bottom();
        }
    }
}

fn handle_mouse(app: &mut VizApp, kind: MouseEventKind, row: u16, column: u16) {
    use super::state::ScrollbarDragTarget;

    let pos = Position::new(column, row);
    let in_graph = app.last_graph_area.contains(pos);
    let in_tab_bar = app.last_tab_bar_area.contains(pos);
    let in_right_content = app.last_right_content_area.contains(pos);
    let in_graph_hscrollbar = app.last_graph_hscrollbar_area.width > 0
        && app.last_graph_hscrollbar_area.contains(pos);

    match kind {
        MouseEventKind::ScrollUp => {
            if in_graph {
                app.record_graph_scroll_activity();
                app.scroll.scroll_up(3);
            } else if in_right_content || in_tab_bar {
                right_panel_scroll_up(app, 3);
            } else {
                app.record_graph_scroll_activity();
                app.scroll.scroll_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_graph {
                app.record_graph_scroll_activity();
                app.scroll.scroll_down(3);
            } else if in_right_content || in_tab_bar {
                right_panel_scroll_down(app, 3);
            } else {
                app.record_graph_scroll_activity();
                app.scroll.scroll_down(3);
            }
        }
        MouseEventKind::ScrollLeft => {
            if in_graph {
                app.record_graph_hscroll_activity();
                app.scroll.scroll_left(3);
            }
        }
        MouseEventKind::ScrollRight => {
            if in_graph {
                app.record_graph_hscroll_activity();
                app.scroll.scroll_right(3);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if in_graph_hscrollbar {
                app.focused_panel = FocusedPanel::Graph;
                app.scrollbar_drag = Some(ScrollbarDragTarget::GraphHorizontal);
                app.record_graph_hscroll_activity();
                hscrollbar_jump_to_column(app, column);
            } else if in_tab_bar {
                // Click on tab header: always focus right panel, switch tab if hit.
                app.focused_panel = FocusedPanel::RightPanel;
                let col_in_tabs = column.saturating_sub(app.last_tab_bar_area.x);
                if let Some(tab) = tab_at_column(col_in_tabs) {
                    app.right_panel_tab = tab;
                }
            } else if in_right_content {
                // Click in right panel content: focus the right panel.
                app.focused_panel = FocusedPanel::RightPanel;
            } else if in_graph {
                // Click in graph: focus graph + select task at clicked line.
                app.focused_panel = FocusedPanel::Graph;
                let content_row = row.saturating_sub(app.last_graph_area.y);
                let visible_idx = app.scroll.offset_y + content_row as usize;
                let line_count = app.visible_line_count();
                if line_count > 0 && visible_idx < line_count {
                    let orig_line = app.visible_to_original(visible_idx);
                    // Guard: orig_line must be within plain_lines range.
                    if orig_line < app.plain_lines.len() {
                        // Check if the click is on the mail indicator (✉) region.
                        let content_col = (column.saturating_sub(app.last_graph_area.x) as usize)
                            + app.scroll.offset_x;
                        let clicked_mail = app
                            .plain_lines
                            .get(orig_line)
                            .and_then(|line| {
                                // Find the ✉ character position in display columns
                                // (not byte offset) since content_col is a visual column.
                                let envelope_char_col =
                                    line.char_indices().position(|(_, c)| c == '✉')?;
                                // The clickable region spans from ✉ through the
                                // count digits/slash that follow it (e.g. "✉3" or "✉2/1").
                                let after_envelope: String = line
                                    .chars()
                                    .skip(envelope_char_col + 1)
                                    .take_while(|c| !c.is_whitespace())
                                    .collect();
                                let end_col =
                                    envelope_char_col + 1 + after_envelope.chars().count();
                                if content_col >= envelope_char_col && content_col < end_col {
                                    Some(())
                                } else {
                                    None
                                }
                            })
                            .is_some();
                        app.select_task_at_line(orig_line);
                        if clicked_mail {
                            // Switch to the Messages tab for this task.
                            app.right_panel_visible = true;
                            app.right_panel_tab = RightPanelTab::Messages;
                            app.invalidate_messages_panel();
                            app.load_messages_panel();
                        }
                    }
                }
            } else if app.last_right_panel_area.contains(pos) {
                // Click on right panel border area: focus right panel.
                app.focused_panel = FocusedPanel::RightPanel;
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.scrollbar_drag == Some(ScrollbarDragTarget::GraphHorizontal) {
                app.record_graph_hscroll_activity();
                hscrollbar_jump_to_column(app, column);
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if app.scrollbar_drag.is_some() {
                app.scrollbar_drag = None;
            }
        }
        _ => {}
    }
}

fn hscrollbar_jump_to_column(app: &mut VizApp, column: u16) {
    let sb = app.last_graph_hscrollbar_area;
    if sb.width == 0 {
        return;
    }
    let max_offset = app
        .scroll
        .content_width
        .saturating_sub(app.scroll.viewport_width);
    if max_offset == 0 {
        return;
    }
    let col_in_track = column.saturating_sub(sb.x) as usize;
    let track_width = sb.width as usize;
    let new_offset = if track_width <= 1 {
        0
    } else {
        (col_in_track * max_offset) / track_width.saturating_sub(1)
    };
    app.scroll.offset_x = new_offset.min(max_offset);
}

/// Enter edit mode for the currently selected config entry.
fn config_enter_edit(app: &mut VizApp) {
    let idx = app.config_panel.selected;
    if idx >= app.config_panel.entries.len() {
        return;
    }

    // Special case: "+ Add endpoint" entry
    if app.config_panel.entries[idx].key == "endpoint.add" {
        app.config_panel.adding_endpoint = true;
        app.config_panel.new_endpoint = super::state::NewEndpointFields::default();
        app.config_panel.new_endpoint_field = 0;
        app.config_panel.editing = false;
        app.input_mode = InputMode::ConfigEdit;
        return;
    }

    // Special case: "Remove endpoint" — just toggle (which triggers removal)
    if app.config_panel.entries[idx].key.ends_with(".remove") {
        app.toggle_config_entry();
        return;
    }

    match &app.config_panel.entries[idx].edit_kind {
        ConfigEditKind::Toggle => {
            app.toggle_config_entry();
        }
        ConfigEditKind::TextInput => {
            app.config_panel.edit_buffer = app.config_panel.entries[idx].value.clone();
            app.config_panel.editing = true;
            app.input_mode = InputMode::ConfigEdit;
        }
        ConfigEditKind::SecretInput => {
            // Start with empty buffer for secrets (don't show masked value)
            app.config_panel.edit_buffer = String::new();
            app.config_panel.editing = true;
            app.input_mode = InputMode::ConfigEdit;
        }
        ConfigEditKind::Choice(choices) => {
            let current = &app.config_panel.entries[idx].value;
            app.config_panel.choice_index = choices.iter().position(|c| c == current).unwrap_or(0);
            app.config_panel.editing = true;
            app.input_mode = InputMode::ConfigEdit;
        }
    }
}

/// Handle key events in ConfigEdit input mode (editing a text field or choosing from a list).
fn handle_config_edit_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    // Add-endpoint form mode
    if app.config_panel.adding_endpoint {
        handle_add_endpoint_input(app, code, modifiers);
        return;
    }

    let idx = app.config_panel.selected;
    if idx >= app.config_panel.entries.len() {
        app.config_panel.editing = false;
        app.input_mode = InputMode::Normal;
        return;
    }

    match &app.config_panel.entries[idx].edit_kind {
        ConfigEditKind::TextInput | ConfigEditKind::SecretInput => match code {
            KeyCode::Esc => {
                app.config_panel.editing = false;
                app.input_mode = InputMode::Normal;
            }
            KeyCode::Enter => {
                app.save_config_entry();
                app.input_mode = InputMode::Normal;
            }
            KeyCode::Backspace => {
                app.config_panel.edit_buffer.pop();
            }
            KeyCode::Char(c) => {
                app.config_panel.edit_buffer.push(c);
            }
            _ => {}
        },
        ConfigEditKind::Choice(choices) => match code {
            KeyCode::Esc => {
                app.config_panel.editing = false;
                app.input_mode = InputMode::Normal;
            }
            KeyCode::Enter => {
                app.save_config_entry();
                app.input_mode = InputMode::Normal;
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if app.config_panel.choice_index > 0 {
                    app.config_panel.choice_index -= 1;
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if app.config_panel.choice_index + 1 < choices.len() {
                    app.config_panel.choice_index += 1;
                }
            }
            _ => {}
        },
        ConfigEditKind::Toggle => {
            app.config_panel.editing = false;
            app.input_mode = InputMode::Normal;
        }
    }
}

/// Handle key events for the add-endpoint form.
fn handle_add_endpoint_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    let field = app.config_panel.new_endpoint_field;

    match code {
        KeyCode::Esc => {
            app.config_panel.adding_endpoint = false;
            app.config_panel.editing = false;
            app.input_mode = InputMode::Normal;
        }
        // Ctrl+S saves the endpoint
        KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
            // Copy edit buffer to current field before saving
            if app.config_panel.editing {
                set_endpoint_field(
                    &mut app.config_panel.new_endpoint,
                    field,
                    &app.config_panel.edit_buffer.clone(),
                );
                app.config_panel.editing = false;
            }
            app.add_endpoint();
            app.input_mode = InputMode::Normal;
        }
        // Tab moves to next field
        KeyCode::Tab => {
            if app.config_panel.editing {
                let buf = app.config_panel.edit_buffer.clone();
                set_endpoint_field(&mut app.config_panel.new_endpoint, field, &buf);
                app.config_panel.editing = false;
            }
            app.config_panel.new_endpoint_field = (field + 1) % 5;
        }
        // BackTab moves to previous field
        KeyCode::BackTab => {
            if app.config_panel.editing {
                let buf = app.config_panel.edit_buffer.clone();
                set_endpoint_field(&mut app.config_panel.new_endpoint, field, &buf);
                app.config_panel.editing = false;
            }
            app.config_panel.new_endpoint_field = if field == 0 { 4 } else { field - 1 };
        }
        KeyCode::Enter => {
            if app.config_panel.editing {
                // Confirm current field value, move to next
                let buf = app.config_panel.edit_buffer.clone();
                set_endpoint_field(&mut app.config_panel.new_endpoint, field, &buf);
                app.config_panel.editing = false;
                if field < 4 {
                    app.config_panel.new_endpoint_field = field + 1;
                } else {
                    // On last field, save the endpoint
                    app.add_endpoint();
                    app.input_mode = InputMode::Normal;
                }
            } else {
                // Start editing this field
                app.config_panel.edit_buffer =
                    get_endpoint_field(&app.config_panel.new_endpoint, field);
                app.config_panel.editing = true;
            }
        }
        KeyCode::Backspace if app.config_panel.editing => {
            app.config_panel.edit_buffer.pop();
        }
        KeyCode::Char(c) if app.config_panel.editing => {
            app.config_panel.edit_buffer.push(c);
        }
        KeyCode::Up | KeyCode::Char('k') if !app.config_panel.editing => {
            app.config_panel.new_endpoint_field = if field == 0 { 4 } else { field - 1 };
        }
        KeyCode::Down | KeyCode::Char('j') if !app.config_panel.editing => {
            app.config_panel.new_endpoint_field = (field + 1) % 5;
        }
        _ => {}
    }
}

/// Set a field on the new-endpoint form by index.
fn set_endpoint_field(fields: &mut super::state::NewEndpointFields, idx: usize, val: &str) {
    match idx {
        0 => fields.name = val.to_string(),
        1 => fields.provider = val.to_string(),
        2 => fields.url = val.to_string(),
        3 => fields.model = val.to_string(),
        4 => fields.api_key = val.to_string(),
        _ => {}
    }
}

/// Get a field from the new-endpoint form by index.
fn get_endpoint_field(fields: &super::state::NewEndpointFields, idx: usize) -> String {
    match idx {
        0 => fields.name.clone(),
        1 => fields.provider.clone(),
        2 => fields.url.clone(),
        3 => fields.model.clone(),
        4 => fields.api_key.clone(),
        _ => String::new(),
    }
}

/// Determine which tab was clicked based on column position within the tab bar.
/// Returns None if the click is on a divider or beyond the last tab.
/// Handle key events for the Files tab.
fn handle_files_key(app: &mut VizApp, code: KeyCode) {
    use super::file_browser::FileBrowserFocus;

    let fb = match app.file_browser.as_mut() {
        Some(fb) => fb,
        None => return,
    };

    // When search mode is active in the tree pane, handle search input first.
    if fb.searching && fb.focus == FileBrowserFocus::Tree {
        match code {
            KeyCode::Esc => {
                fb.exit_search();
            }
            KeyCode::Backspace => {
                fb.search_pop();
            }
            // Allow navigating the filtered tree while searching
            KeyCode::Up => {
                fb.tree_state.key_up();
                fb.load_preview();
            }
            KeyCode::Down => {
                fb.tree_state.key_down();
                fb.load_preview();
            }
            KeyCode::Enter => {
                // Confirm search: exit search input mode but keep the filter
                fb.searching = false;
            }
            KeyCode::Char(ch) => {
                fb.search_push(ch);
            }
            _ => {}
        }
        return;
    }

    match fb.focus {
        FileBrowserFocus::Tree => match code {
            // '/' enters search mode
            KeyCode::Char('/') => {
                fb.enter_search();
            }
            // Tab: switch focus to preview pane
            KeyCode::Tab => {
                fb.focus = FileBrowserFocus::Preview;
            }
            // Navigation: move selection up/down
            KeyCode::Up | KeyCode::Char('k') => {
                fb.tree_state.key_up();
                fb.load_preview();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                fb.tree_state.key_down();
                fb.load_preview();
            }
            // Expand / open: Enter, l, Right arrow
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                // If it's a directory, expand it. If file, just load preview.
                let selected = fb.tree_state.selected().to_vec();
                if !selected.is_empty() {
                    let mut path = fb.root.clone();
                    for seg in &selected {
                        path.push(seg);
                    }
                    if path.is_dir() {
                        fb.tree_state.toggle_selected();
                    }
                }
                fb.load_preview();
            }
            // Collapse / parent: Backspace, h, Left arrow
            KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left => {
                fb.tree_state.key_left();
                fb.load_preview();
            }
            // Toggle expand/collapse without moving
            KeyCode::Char(' ') => {
                fb.tree_state.toggle_selected();
            }
            // Jump to first/last
            KeyCode::Home => {
                fb.tree_state.select_first();
                fb.load_preview();
            }
            KeyCode::End => {
                fb.tree_state.select_last();
                fb.load_preview();
            }
            // Page up/down for tree
            KeyCode::PageUp => {
                for _ in 0..10 {
                    fb.tree_state.key_up();
                }
                fb.load_preview();
            }
            KeyCode::PageDown => {
                for _ in 0..10 {
                    fb.tree_state.key_down();
                }
                fb.load_preview();
            }
            _ => {}
        },
        FileBrowserFocus::Preview => match code {
            // Tab: switch focus back to tree pane
            KeyCode::Tab => {
                fb.focus = FileBrowserFocus::Tree;
            }
            // Scroll preview
            KeyCode::Up | KeyCode::Char('k') => {
                fb.preview_scroll_up(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                fb.preview_scroll_down(1);
            }
            KeyCode::PageUp => {
                fb.preview_scroll_up(20);
            }
            KeyCode::PageDown => {
                fb.preview_scroll_down(20);
            }
            // Jump to top/bottom
            KeyCode::Char('g') => {
                fb.preview_go_top();
            }
            KeyCode::Char('G') => {
                fb.preview_go_bottom();
            }
            KeyCode::Home => {
                fb.preview_go_top();
            }
            KeyCode::End => {
                fb.preview_go_bottom();
            }
            _ => {}
        },
    }
}

/// Determine which tab was clicked based on column position within the tab bar.
/// Returns None if the click is on a divider or beyond the last tab.
fn tab_at_column(col: u16) -> Option<RightPanelTab> {
    let labels = [
        "0:Chat", "1:Detail", "2:Log", "3:Msg", "4:Agency", "5:Config", "6:Files",
    ];
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
