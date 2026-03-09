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
    CommandEffect, ConfigEditKind, ConfirmAction, ControlPanelFocus, FocusedPanel, InputMode,
    InspectorSubFocus, RightPanelTab, TaskFormField, TextPromptAction, VizApp,
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

    // Service control panel intercepts keys when open
    if app.service_health.panel_open {
        handle_service_control_panel_key(app, code);
        return;
    }

    // Service health detail popup intercepts keys when open
    if app.service_health.detail_open {
        match code {
            KeyCode::Esc | KeyCode::Char('q') => app.service_health.detail_open = false,
            KeyCode::Down | KeyCode::Char('j') => {
                app.service_health.detail_scroll =
                    app.service_health.detail_scroll.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.service_health.detail_scroll =
                    app.service_health.detail_scroll.saturating_sub(1);
            }
            _ => {}
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
            super::state::paste_insert_mode(text, &mut app.chat.editor);
        }
        InputMode::Search => {
            // Strip newlines for search — it's single-line.
            let clean: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            app.search_input.push_str(&clean);
            app.update_search();
        }
        InputMode::TextPrompt(_action) => {
            super::state::paste_insert_mode(text, &mut app.text_prompt.editor);
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
            // Insert pasted text at cursor position, preserving newlines (like chat).
            super::state::paste_insert_mode(text, &mut app.messages_panel.editor);
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

/// Handle keyboard input when the service control panel is open.
fn handle_service_control_panel_key(app: &mut VizApp, code: KeyCode) {
    let stuck_count = app.service_health.stuck_tasks.len();
    if app.service_health.panic_confirm {
        match code {
            KeyCode::Char('y') => {
                app.execute_panic_kill();
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                app.service_health.panic_confirm = false;
            }
            _ => {}
        }
        return;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') => {
            app.close_service_control_panel();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            app.service_health.panel_focus = app.service_health.panel_focus.next(stuck_count);
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.service_health.panel_focus = app.service_health.panel_focus.prev(stuck_count);
        }
        KeyCode::Enter | KeyCode::Char(' ') => {
            if app.service_health.panel_focus == ControlPanelFocus::PanicKill {
                app.service_health.panic_confirm = true;
            } else {
                app.execute_service_action();
            }
        }
        KeyCode::Char('s') | KeyCode::Char('S') => {
            app.service_health.panel_focus = ControlPanelFocus::StartStop;
            app.execute_service_action();
        }
        KeyCode::Char('p') | KeyCode::Char('P') => {
            app.service_health.panel_focus = ControlPanelFocus::PauseResume;
            app.execute_service_action();
        }
        KeyCode::Char('K') => {
            app.service_health.panel_focus = ControlPanelFocus::PanicKill;
            app.service_health.panic_confirm = true;
        }
        KeyCode::Char('u') | KeyCode::Char('U') => {
            if let ControlPanelFocus::StuckAgent(idx) = app.service_health.panel_focus
                && let Some(st) = app.service_health.stuck_tasks.get(idx)
            {
                let tid = st.task_id.clone();
                app.exec_command(
                    vec!["unclaim".to_string(), tid.clone()],
                    CommandEffect::RefreshAndNotify(format!("Unclaimed {}", tid)),
                );
                app.set_service_feedback(format!("Unclaimed {}", tid));
            }
        }
        _ => {}
    }
}

fn handle_text_prompt_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    use super::state::{editor_clear, editor_text};
    use crossterm::event::KeyEvent;
    let action = match &app.input_mode {
        InputMode::TextPrompt(a) => a.clone(),
        _ => return,
    };
    let is_multiline = matches!(action, TextPromptAction::EditDescription(_));
    let submit = match code {
        KeyCode::Enter if is_multiline && modifiers.contains(KeyModifiers::CONTROL) => true,
        KeyCode::Enter if !is_multiline => true,
        _ => false,
    };
    if submit {
        let text = editor_text(&app.text_prompt.editor);
        editor_clear(&mut app.text_prompt.editor);
        if text.trim().is_empty() {
            if action == TextPromptAction::AttachFile {
                app.input_mode = InputMode::ChatInput;
                app.inspector_sub_focus = InspectorSubFocus::TextEntry;
            } else {
                app.input_mode = InputMode::Normal;
            }
            return;
        }
        match action {
            TextPromptAction::MarkFailed(task_id) => {
                app.exec_command(
                    vec!["fail".into(), task_id.clone(), "--reason".into(), text],
                    CommandEffect::RefreshAndNotify(format!("Marked '{}' failed", task_id)),
                );
            }
            TextPromptAction::SendMessage(task_id) => {
                app.exec_command(
                    vec![
                        "msg".into(),
                        "send".into(),
                        task_id.clone(),
                        text,
                        "--from".into(),
                        "tui".into(),
                    ],
                    CommandEffect::Notify(format!("Message sent to '{}'", task_id)),
                );
            }
            TextPromptAction::EditDescription(task_id) => {
                app.exec_command(
                    vec!["edit".into(), task_id.clone(), "-d".into(), text],
                    CommandEffect::RefreshAndNotify(format!("Updated '{}'", task_id)),
                );
            }
            TextPromptAction::AttachFile => {
                app.attach_file(&text);
                app.input_mode = InputMode::ChatInput;
                app.inspector_sub_focus = InspectorSubFocus::TextEntry;
                return;
            }
        }
        app.input_mode = InputMode::Normal;
        return;
    }
    match code {
        KeyCode::Esc => {
            editor_clear(&mut app.text_prompt.editor);
            if action == TextPromptAction::AttachFile {
                app.input_mode = InputMode::ChatInput;
                app.inspector_sub_focus = InspectorSubFocus::TextEntry;
            } else {
                app.input_mode = InputMode::Normal;
            }
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            editor_clear(&mut app.text_prompt.editor);
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {}
        _ => {
            if code == KeyCode::Enter
                && (is_multiline
                    || modifiers.contains(KeyModifiers::SHIFT)
                    || modifiers.contains(KeyModifiers::ALT))
            {
                app.editor_handler.on_key_event(
                    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                    &mut app.text_prompt.editor,
                );
            } else {
                app.editor_handler
                    .on_key_event(KeyEvent::new(code, modifiers), &mut app.text_prompt.editor);
            }
        }
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
    use super::state::{editor_clear, editor_text};
    use crossterm::event::KeyEvent;
    match code {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            app.chat_input_dismissed = true;
            app.inspector_sub_focus = InspectorSubFocus::ChatHistory;
            return;
        }
        KeyCode::Enter
            if !modifiers.contains(KeyModifiers::SHIFT)
                && !modifiers.contains(KeyModifiers::ALT) =>
        {
            let text = editor_text(&app.chat.editor);
            editor_clear(&mut app.chat.editor);
            if !text.trim().is_empty() {
                app.send_chat_message(text);
            }
            return;
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            editor_clear(&mut app.chat.editor);
            app.input_mode = InputMode::Normal;
            app.inspector_sub_focus = InspectorSubFocus::ChatHistory;
            return;
        }
        KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.try_paste_clipboard_image();
            return;
        }
        KeyCode::Up if modifiers.contains(KeyModifiers::ALT) => {
            app.record_panel_scroll_activity();
            app.chat.scroll = app.chat.scroll.saturating_add(1);
            return;
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::ALT) => {
            app.record_panel_scroll_activity();
            app.chat.scroll = app.chat.scroll.saturating_sub(1);
            return;
        }
        _ => {}
    }
    if code == KeyCode::Enter
        && (modifiers.contains(KeyModifiers::SHIFT) || modifiers.contains(KeyModifiers::ALT))
    {
        app.editor_handler.on_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app.chat.editor,
        );
        return;
    }
    app.editor_handler
        .on_key_event(KeyEvent::new(code, modifiers), &mut app.chat.editor);
}

fn handle_message_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    use super::state::{editor_clear, editor_text};
    use crossterm::event::KeyEvent;
    match code {
        KeyCode::Esc => {
            // Save draft on exit so it persists across panel/task switches.
            app.save_message_draft();
            app.input_mode = InputMode::Normal;
            return;
        }
        KeyCode::Enter
            if !modifiers.contains(KeyModifiers::SHIFT)
                && !modifiers.contains(KeyModifiers::ALT) =>
        {
            let text = editor_text(&app.messages_panel.editor);
            editor_clear(&mut app.messages_panel.editor);
            if !text.trim().is_empty()
                && let Some(task_id) = app.messages_panel.task_id.clone()
            {
                // Clear draft on successful send.
                app.message_drafts.remove(&task_id);
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
                app.invalidate_messages_panel();
                app.load_messages_panel();
            }
            return;
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            editor_clear(&mut app.messages_panel.editor);
            // Clear draft on Ctrl+C (intentional discard).
            if let Some(task_id) = &app.messages_panel.task_id {
                app.message_drafts.remove(task_id);
            }
            app.input_mode = InputMode::Normal;
            return;
        }
        KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.try_paste_clipboard_image();
            return;
        }
        KeyCode::Up if modifiers.contains(KeyModifiers::ALT) => {
            app.record_panel_scroll_activity();
            app.messages_panel.scroll = app.messages_panel.scroll.saturating_sub(1);
            return;
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::ALT) => {
            app.record_panel_scroll_activity();
            app.messages_panel.scroll = app.messages_panel.scroll.saturating_add(1);
            return;
        }
        _ => {}
    }
    if code == KeyCode::Enter
        && (modifiers.contains(KeyModifiers::SHIFT) || modifiers.contains(KeyModifiers::ALT))
    {
        app.editor_handler.on_key_event(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app.messages_panel.editor,
        );
        return;
    }
    app.editor_handler.on_key_event(
        KeyEvent::new(code, modifiers),
        &mut app.messages_panel.editor,
    );
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

        // Period: toggle system task visibility
        KeyCode::Char('.') => {
            app.show_system_tasks = !app.show_system_tasks;
            app.system_tasks_just_toggled = true;
            app.force_refresh();
        }

        // Backslash: toggle right panel visibility
        KeyCode::Char('\\') => {
            app.toggle_right_panel();
        }

        // Cycle inspector size: 1/3 → 1/2 → 2/3 → full → off
        KeyCode::Char('=') | KeyCode::BackTab => {
            app.cycle_layout_mode();
        }
        // Grow viz pane by ~5%
        KeyCode::Char('i') => {
            app.grow_viz_pane();
        }
        // Shrink viz pane by ~5%
        KeyCode::Char('I') => {
            app.shrink_viz_pane();
        }

        // Navigate between matches
        KeyCode::Char('n') => app.next_match(),
        KeyCode::Char('N') => app.prev_match(),

        // Alt+Up/Down: toggle focus between graph and right panel
        KeyCode::Up if modifiers.contains(KeyModifiers::ALT) => {
            app.toggle_panel_focus();
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::ALT) => {
            app.toggle_panel_focus();
        }

        // Alt+Left/Right: cycle inspector views with slide animation
        KeyCode::Left if modifiers.contains(KeyModifiers::ALT) => {
            app.cycle_inspector_view_backward();
        }
        KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => {
            app.cycle_inspector_view_forward();
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
                super::state::editor_clear(&mut app.text_prompt.editor);
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
                super::state::editor_clear(&mut app.text_prompt.editor);
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
            app.inspector_sub_focus = InspectorSubFocus::TextEntry;
        }

        // Digit keys 0-7: switch right panel tab
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
                KeyCode::Char('=') | KeyCode::BackTab => app.cycle_layout_mode(),
                KeyCode::Char('i') => app.grow_viz_pane(),
                KeyCode::Char('I') => app.shrink_viz_pane(),
                KeyCode::Esc => {
                    app.focused_panel = FocusedPanel::Graph;
                }
                KeyCode::Char(d @ '0'..='7') => {
                    let idx = (d as u8 - b'0') as usize;
                    if let Some(tab) = RightPanelTab::from_index(idx) {
                        app.right_panel_tab = tab;
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
        }

        // Backslash: toggle right panel
        KeyCode::Char('\\') => {
            app.toggle_right_panel();
        }

        // Cycle inspector size: 1/3 → 1/2 → 2/3 → full → off
        KeyCode::Char('=') | KeyCode::BackTab => {
            app.cycle_layout_mode();
        }
        // Grow/shrink viz pane by ~5%
        KeyCode::Char('i') => {
            app.grow_viz_pane();
        }
        KeyCode::Char('I') => {
            app.shrink_viz_pane();
        }

        // Esc: go back to graph focus
        KeyCode::Esc => {
            app.focused_panel = FocusedPanel::Graph;
        }

        // Number keys 0-6 switch tabs
        KeyCode::Char(d @ '0'..='7') => {
            let idx = (d as u8 - b'0') as usize;
            if let Some(tab) = RightPanelTab::from_index(idx) {
                app.right_panel_tab = tab;
            }
        }

        // Alt+Up/Down: toggle panel focus
        KeyCode::Up if modifiers.contains(KeyModifiers::ALT) => {
            app.toggle_panel_focus();
        }
        KeyCode::Down if modifiers.contains(KeyModifiers::ALT) => {
            app.toggle_panel_focus();
        }

        // Alt+Left/Right: cycle inspector views with slide animation
        KeyCode::Left if modifiers.contains(KeyModifiers::ALT) => {
            app.cycle_inspector_view_backward();
        }
        KeyCode::Right if modifiers.contains(KeyModifiers::ALT) => {
            app.cycle_inspector_view_forward();
        }

        // Left/Right cycle tabs
        KeyCode::Left => {
            app.right_panel_tab = app.right_panel_tab.prev();
        }
        KeyCode::Right => {
            app.right_panel_tab = app.right_panel_tab.next();
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
                app.inspector_sub_focus = InspectorSubFocus::TextEntry;
                // Editor cursor is already at the right position.
            } else if app.right_panel_tab == RightPanelTab::Messages {
                // Enter compose mode without clearing — preserves any draft.
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

        // Chat tab: '[' / ']' cycle between coordinator tabs
        KeyCode::Char('[') if app.right_panel_tab == RightPanelTab::Chat => {
            let ids = app.list_coordinator_ids();
            if ids.len() > 1 {
                let pos = ids.iter().position(|&id| id == app.active_coordinator_id).unwrap_or(0);
                let prev = if pos == 0 { ids.len() - 1 } else { pos - 1 };
                app.switch_coordinator(ids[prev]);
            }
        }
        KeyCode::Char(']') if app.right_panel_tab == RightPanelTab::Chat => {
            let ids = app.list_coordinator_ids();
            if ids.len() > 1 {
                let pos = ids.iter().position(|&id| id == app.active_coordinator_id).unwrap_or(0);
                let next = (pos + 1) % ids.len();
                app.switch_coordinator(ids[next]);
            }
        }
        // Chat tab: '+' creates a new coordinator session
        KeyCode::Char('+') if app.right_panel_tab == RightPanelTab::Chat => {
            app.create_coordinator(None);
        }

        // Detail tab: 'R' toggles raw JSON display
        KeyCode::Char('R') if app.right_panel_tab == RightPanelTab::Detail => {
            app.detail_raw_json = !app.detail_raw_json;
            app.hud_detail = None; // force reload with new format
            app.load_hud_detail();
        }

        // Detail tab: Space toggles section collapse at current scroll position
        KeyCode::Char(' ') if app.right_panel_tab == RightPanelTab::Detail => {
            app.toggle_detail_section_at_scroll();
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
        RightPanelTab::Firehose => {
            app.firehose.auto_tail = false;
            app.firehose.scroll = app.firehose.scroll.saturating_sub(amount);
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
        RightPanelTab::Firehose => {
            app.firehose.scroll += amount;
            let max = app
                .firehose
                .total_rendered_lines
                .saturating_sub(app.firehose.viewport_height);
            if app.firehose.scroll >= max {
                app.firehose.auto_tail = true;
            }
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
        RightPanelTab::Firehose => {
            app.firehose.auto_tail = false;
            app.firehose.scroll = 0;
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
        RightPanelTab::Firehose => {
            app.firehose.auto_tail = true;
            app.firehose.scroll = usize::MAX;
        }
    }
}

fn handle_mouse(app: &mut VizApp, kind: MouseEventKind, row: u16, column: u16) {
    use super::state::ScrollbarDragTarget;

    let pos = Position::new(column, row);

    // When a text prompt overlay is visible, intercept scroll events on it.
    let in_text_prompt = app.last_text_prompt_area.width > 0
        && app.last_text_prompt_area.contains(pos)
        && matches!(app.input_mode, InputMode::TextPrompt(_));

    let in_graph = app.last_graph_area.contains(pos);
    let in_tab_bar = app.last_tab_bar_area.contains(pos);
    let in_right_content = app.last_right_content_area.contains(pos);
    let in_graph_hscrollbar =
        app.last_graph_hscrollbar_area.width > 0 && app.last_graph_hscrollbar_area.contains(pos);
    let in_graph_vscrollbar =
        app.last_graph_scrollbar_area.height > 0 && app.last_graph_scrollbar_area.contains(pos);
    let in_panel_vscrollbar =
        app.last_panel_scrollbar_area.height > 0 && app.last_panel_scrollbar_area.contains(pos);

    match kind {
        MouseEventKind::ScrollUp => {
            if in_text_prompt {
                // Scroll up in text prompt: move cursor up to trigger viewport change.
                scroll_editor_up(app, 3, EditorTarget::TextPrompt);
            } else if in_graph {
                app.record_graph_scroll_activity();
                app.scroll.scroll_up(3);
            } else if (in_right_content || in_tab_bar)
                && app.right_panel_tab == RightPanelTab::Files
                && app.last_file_tree_area.height > 0
            {
                // Files tab: scroll tree or preview depending on mouse position.
                app.record_panel_scroll_activity();
                if app.last_file_preview_area.contains(pos) {
                    if let Some(fb) = app.file_browser.as_mut() {
                        fb.preview_scroll_up(3);
                    }
                } else if let Some(fb) = app.file_browser.as_mut() {
                    fb.tree_state.scroll_up(3);
                }
            } else if in_right_content || in_tab_bar {
                right_panel_scroll_up(app, 3);
            } else {
                app.record_graph_scroll_activity();
                app.scroll.scroll_up(3);
            }
        }
        MouseEventKind::ScrollDown => {
            if in_text_prompt {
                // Scroll down in text prompt: move cursor down to trigger viewport change.
                scroll_editor_down(app, 3, EditorTarget::TextPrompt);
            } else if in_graph {
                app.record_graph_scroll_activity();
                app.scroll.scroll_down(3);
            } else if (in_right_content || in_tab_bar)
                && app.right_panel_tab == RightPanelTab::Files
                && app.last_file_tree_area.height > 0
            {
                // Files tab: scroll tree or preview depending on mouse position.
                app.record_panel_scroll_activity();
                if app.last_file_preview_area.contains(pos) {
                    if let Some(fb) = app.file_browser.as_mut() {
                        fb.preview_scroll_down(3);
                    }
                } else if let Some(fb) = app.file_browser.as_mut() {
                    fb.tree_state.scroll_down(3);
                }
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
            // Service health badge click
            let in_service_badge =
                app.last_service_badge_area.width > 0 && app.last_service_badge_area.contains(pos);
            if in_service_badge {
                app.toggle_service_control_panel();
                return;
            }
            if in_graph_vscrollbar {
                // Click on graph vertical scrollbar: start drag and jump.
                app.focused_panel = FocusedPanel::Graph;
                app.scrollbar_drag = Some(ScrollbarDragTarget::Graph);
                app.record_graph_scroll_activity();
                vscrollbar_jump_graph(app, row);
            } else if in_panel_vscrollbar {
                // Click on panel vertical scrollbar: start drag and jump.
                app.focused_panel = FocusedPanel::RightPanel;
                app.scrollbar_drag = Some(ScrollbarDragTarget::Panel);
                app.record_panel_scroll_activity();
                vscrollbar_jump_panel(app, row);
            } else if in_graph_hscrollbar {
                app.focused_panel = FocusedPanel::Graph;
                app.scrollbar_drag = Some(ScrollbarDragTarget::GraphHorizontal);
                app.record_graph_hscroll_activity();
                hscrollbar_jump_to_column(app, column);
            } else if in_text_prompt {
                // Click inside text prompt overlay: position cursor via edtui.
                route_mouse_to_editor(app, row, column, EditorTarget::TextPrompt);
            } else if in_tab_bar {
                // Click on tab header: always focus right panel, switch tab if hit.
                app.focused_panel = FocusedPanel::RightPanel;
                let col_in_tabs = column.saturating_sub(app.last_tab_bar_area.x);
                if let Some(tab) = tab_at_column(col_in_tabs) {
                    app.right_panel_tab = tab;
                }
            } else if app.last_chat_input_area.height > 0
                && app.last_chat_input_area.contains(pos)
                && (app.right_panel_tab == RightPanelTab::Chat)
            {
                // Click on chat input area: enter/resume editing and position cursor.
                app.focused_panel = FocusedPanel::RightPanel;
                app.chat_input_dismissed = false;
                app.input_mode = InputMode::ChatInput;
                app.inspector_sub_focus = InspectorSubFocus::TextEntry;
                route_mouse_to_editor(app, row, column, EditorTarget::Chat);
            } else if app.last_message_input_area.height > 0
                && app.last_message_input_area.contains(pos)
                && (app.right_panel_tab == RightPanelTab::Messages)
            {
                // Click on message input area: enter/resume editing and position cursor.
                app.focused_panel = FocusedPanel::RightPanel;
                app.input_mode = InputMode::MessageInput;
                route_mouse_to_editor(app, row, column, EditorTarget::Message);
            } else if in_right_content
                && app.right_panel_tab == RightPanelTab::Chat
                && app.last_chat_message_area.height > 0
                && app.last_chat_message_area.contains(pos)
            {
                // Click on chat message history area: focus history, exit text editing.
                app.focused_panel = FocusedPanel::RightPanel;
                app.inspector_sub_focus = InspectorSubFocus::ChatHistory;
                if app.input_mode == InputMode::ChatInput {
                    app.input_mode = InputMode::Normal;
                    app.chat_input_dismissed = true;
                }
            } else if in_right_content
                && app.right_panel_tab == RightPanelTab::Files
                && app.last_file_tree_area.height > 0
            {
                // Click in Files tab.
                app.focused_panel = FocusedPanel::RightPanel;
                if app.last_file_tree_area.contains(pos) {
                    // Click on the tree pane: select the clicked item.
                    if let Some(fb) = app.file_browser.as_mut() {
                        fb.focus = super::file_browser::FileBrowserFocus::Tree;
                        fb.tree_state.click_at(pos);
                        fb.load_preview();
                    }
                } else if app.last_file_preview_area.contains(pos) {
                    // Click on the preview pane: switch focus to preview.
                    if let Some(fb) = app.file_browser.as_mut() {
                        fb.focus = super::file_browser::FileBrowserFocus::Preview;
                    }
                }
            } else if in_right_content && app.right_panel_tab == RightPanelTab::Detail {
                // Click in Detail tab: toggle section collapse if clicking a header.
                app.focused_panel = FocusedPanel::RightPanel;
                let content_row = row.saturating_sub(app.last_right_content_area.y) as usize;
                app.toggle_detail_section_at_screen_row(content_row);
            } else if in_right_content {
                // Click in right panel content: focus the right panel.
                app.focused_panel = FocusedPanel::RightPanel;
                // Config tab: click to select an entry.
                if app.right_panel_tab == RightPanelTab::Config
                    && !app.config_panel.editing
                    && let Some(&(entry_idx, _)) =
                        app.config_entry_y_positions.iter().find(|(_, y)| *y == row)
                {
                    app.config_panel.selected = entry_idx;
                }
            } else if in_graph {
                // Click in graph: focus graph + select task at clicked line.
                app.focused_panel = FocusedPanel::Graph;
                // Start drag-to-pan tracking for touch/mouse pan gestures.
                app.graph_pan_last = Some((column, row));
                // Exit text entry mode if active (text persists, goes gray).
                if app.input_mode == InputMode::ChatInput {
                    app.input_mode = InputMode::Normal;
                    app.chat_input_dismissed = true;
                    app.inspector_sub_focus = InspectorSubFocus::ChatHistory;
                } else if app.input_mode == InputMode::MessageInput {
                    app.save_message_draft();
                    app.input_mode = InputMode::Normal;
                }
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
                        } else if let Some(line) = app.plain_lines.get(orig_line) {
                            // Determine click region for tab switching.
                            let chars: Vec<char> = line.chars().collect();
                            let text_start = chars.iter().position(|c| c.is_alphanumeric());
                            // Find the "  (" separator between task ID and status.
                            let paren_start = text_start.and_then(|ts| {
                                (ts..chars.len().saturating_sub(1))
                                    .find(|&i| chars[i] == ' ' && chars[i + 1] == '(')
                            });
                            if let (Some(ts), Some(ps)) = (text_start, paren_start)
                                && app.right_panel_visible
                            {
                                // Inspector already open — update which tab is shown.
                                if content_col >= ts && content_col < ps {
                                    app.right_panel_tab = RightPanelTab::Detail;
                                } else if content_col >= ps {
                                    app.right_panel_tab = RightPanelTab::Log;
                                    app.invalidate_log_pane();
                                    app.load_log_pane();
                                }
                            }
                            // If inspector is closed, just select — don't auto-open.
                        }
                    }
                }
            } else if app.last_right_panel_area.contains(pos) {
                // Click on right panel border area: focus right panel.
                app.focused_panel = FocusedPanel::RightPanel;
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.scrollbar_drag == Some(ScrollbarDragTarget::Graph) {
                app.record_graph_scroll_activity();
                vscrollbar_jump_graph(app, row);
            } else if app.scrollbar_drag == Some(ScrollbarDragTarget::Panel) {
                app.record_panel_scroll_activity();
                vscrollbar_jump_panel(app, row);
            } else if app.scrollbar_drag == Some(ScrollbarDragTarget::GraphHorizontal) {
                app.record_graph_hscroll_activity();
                hscrollbar_jump_to_column(app, column);
            } else if let Some((prev_col, prev_row)) = app.graph_pan_last {
                // Drag-to-pan: move the graph viewport following the finger/mouse.
                // Natural scrolling: dragging down (row increases) scrolls content up.
                let dx = prev_col as i32 - column as i32;
                let dy = prev_row as i32 - row as i32;
                if dx > 0 {
                    app.record_graph_hscroll_activity();
                    app.scroll.scroll_right(dx as usize);
                } else if dx < 0 {
                    app.record_graph_hscroll_activity();
                    app.scroll.scroll_left((-dx) as usize);
                }
                if dy > 0 {
                    app.record_graph_scroll_activity();
                    app.scroll.scroll_down(dy as usize);
                } else if dy < 0 {
                    app.record_graph_scroll_activity();
                    app.scroll.scroll_up((-dy) as usize);
                }
                app.graph_pan_last = Some((column, row));
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if app.scrollbar_drag.is_some() {
                app.scrollbar_drag = None;
            }
            app.graph_pan_last = None;
        }
        _ => {}
    }
}

/// Which editor should receive a mouse event.
pub(super) enum EditorTarget {
    Chat,
    TextPrompt,
    Message,
}

/// Route a mouse-down event to the appropriate edtui editor for click-to-position.
///
/// For the chat editor (which uses our custom `render_editor_word_wrap` instead of
/// edtui's `EditorView`), we bypass `on_mouse_event` entirely because edtui's
/// coordinate mapping relies on `screen_area` which is never set by our renderer.
/// Instead, we compute the cursor position ourselves using the same word-wrapping
/// logic as the renderer.
pub(super) fn route_mouse_to_editor(app: &mut VizApp, row: u16, column: u16, target: EditorTarget) {
    match target {
        EditorTarget::Chat => {
            chat_click_to_cursor(app, row, column);
        }
        EditorTarget::TextPrompt => {
            let mouse_event = crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
            app.editor_handler
                .on_mouse_event(mouse_event, &mut app.text_prompt.editor);
        }
        EditorTarget::Message => {
            message_click_to_cursor(app, row, column);
        }
    }
}

/// Map a screen-space mouse click to a logical cursor position in the chat editor.
///
/// This replicates the coordinate logic from `render_editor_word_wrap` and
/// `draw_chat_input` to correctly handle the separator line, "> " prefix, and
/// word-boundary wrapping.
fn chat_click_to_cursor(app: &mut VizApp, screen_row: u16, screen_col: u16) {
    use unicode_width::UnicodeWidthChar;

    let input_area = app.last_chat_input_area;
    if input_area.width == 0 || input_area.height == 0 {
        return;
    }

    // Editor area matches draw_chat_input layout: separator at y, editor at y+1, prefix "> ".
    let prefix_len: u16 = 2;
    let editor_y = if input_area.height >= 2 {
        input_area.y + 1
    } else {
        input_area.y
    };
    let editor_x = input_area.x + prefix_len;
    let editor_width = input_area.width.saturating_sub(prefix_len) as usize;
    let editor_height = if input_area.height >= 2 {
        input_area.height - 1
    } else {
        input_area.height
    } as usize;

    if editor_width == 0 || editor_height == 0 {
        return;
    }

    // Convert screen coords to editor-local.
    let local_row = screen_row.saturating_sub(editor_y) as usize;
    let local_col = screen_col.saturating_sub(editor_x) as usize;

    // Build visual row table and compute scroll offset (same algorithm as renderer).
    let text = app.chat.editor.lines.to_string();
    let logical_lines: Vec<&str> = text.split('\n').collect();

    // Each entry: (logical_line_idx, segment_char_start, segment_char_end)
    let mut visual_rows: Vec<(usize, usize, usize)> = Vec::new();
    let mut cursor_visual_row = 0usize;

    for (line_idx, logical_line) in logical_lines.iter().enumerate() {
        let segments = super::render::word_wrap_segments(logical_line, editor_width);
        if line_idx == app.chat.editor.cursor.row {
            let (sub_row, _) =
                super::render::cursor_in_segments(&segments, app.chat.editor.cursor.col);
            cursor_visual_row = visual_rows.len() + sub_row;
        }
        for &(start, end) in &segments {
            visual_rows.push((line_idx, start, end));
        }
    }

    // Scroll offset (same as render_editor_word_wrap).
    let scroll = if cursor_visual_row >= editor_height {
        cursor_visual_row - editor_height + 1
    } else {
        0
    };

    // The clicked visual row, accounting for scroll.
    let target_visual = scroll + local_row;

    if target_visual < visual_rows.len() {
        let (line_idx, seg_start, seg_end) = visual_rows[target_visual];
        let chars: Vec<char> = logical_lines[line_idx].chars().collect();

        // Walk chars in this segment to find the char index matching the click column.
        let mut char_idx = seg_start;
        let mut display_col = 0usize;
        while char_idx < seg_end {
            let cw = UnicodeWidthChar::width(chars[char_idx]).unwrap_or(0);
            if display_col + cw > local_col {
                break;
            }
            display_col += cw;
            char_idx += 1;
        }

        app.chat.editor.cursor = edtui::Index2::new(line_idx, char_idx);
    } else {
        // Click beyond content: place cursor at end of last line.
        if let Some(last_line) = logical_lines.last() {
            app.chat.editor.cursor = edtui::Index2::new(
                logical_lines.len().saturating_sub(1),
                last_line.chars().count(),
            );
        }
    }
}

/// Map a screen-space mouse click to a logical cursor position in the message editor.
/// Same coordinate logic as `chat_click_to_cursor` but using the message input area.
fn message_click_to_cursor(app: &mut VizApp, screen_row: u16, screen_col: u16) {
    use unicode_width::UnicodeWidthChar;

    let input_area = app.last_message_input_area;
    if input_area.width == 0 || input_area.height == 0 {
        return;
    }

    let prefix_len: u16 = 2;
    let editor_y = if input_area.height >= 2 {
        input_area.y + 1
    } else {
        input_area.y
    };
    let editor_x = input_area.x + prefix_len;
    let editor_width = input_area.width.saturating_sub(prefix_len) as usize;
    let editor_height = if input_area.height >= 2 {
        input_area.height - 1
    } else {
        input_area.height
    } as usize;

    if editor_width == 0 || editor_height == 0 {
        return;
    }

    let local_row = screen_row.saturating_sub(editor_y) as usize;
    let local_col = screen_col.saturating_sub(editor_x) as usize;

    let text = app.messages_panel.editor.lines.to_string();
    let logical_lines: Vec<&str> = text.split('\n').collect();

    let mut visual_rows: Vec<(usize, usize, usize)> = Vec::new();
    let mut cursor_visual_row = 0usize;

    for (line_idx, logical_line) in logical_lines.iter().enumerate() {
        let segments = super::render::word_wrap_segments(logical_line, editor_width);
        if line_idx == app.messages_panel.editor.cursor.row {
            let (sub_row, _) =
                super::render::cursor_in_segments(&segments, app.messages_panel.editor.cursor.col);
            cursor_visual_row = visual_rows.len() + sub_row;
        }
        for &(start, end) in &segments {
            visual_rows.push((line_idx, start, end));
        }
    }

    let scroll = if cursor_visual_row >= editor_height {
        cursor_visual_row - editor_height + 1
    } else {
        0
    };

    let target_visual = scroll + local_row;

    if target_visual < visual_rows.len() {
        let (line_idx, seg_start, seg_end) = visual_rows[target_visual];
        let chars: Vec<char> = logical_lines[line_idx].chars().collect();

        let mut char_idx = seg_start;
        let mut display_col = 0usize;
        while char_idx < seg_end {
            let cw = UnicodeWidthChar::width(chars[char_idx]).unwrap_or(0);
            if display_col + cw > local_col {
                break;
            }
            display_col += cw;
            char_idx += 1;
        }

        app.messages_panel.editor.cursor = edtui::Index2::new(line_idx, char_idx);
    } else if let Some(last_line) = logical_lines.last() {
        app.messages_panel.editor.cursor = edtui::Index2::new(
            logical_lines.len().saturating_sub(1),
            last_line.chars().count(),
        );
    }
}

/// Scroll an editor up by moving the cursor up `n` lines.
fn scroll_editor_up(app: &mut VizApp, n: usize, target: EditorTarget) {
    use crossterm::event::KeyEvent;
    for _ in 0..n {
        let key = KeyEvent::new(KeyCode::Up, crossterm::event::KeyModifiers::NONE);
        match target {
            EditorTarget::Chat => {
                app.editor_handler.on_key_event(key, &mut app.chat.editor);
            }
            EditorTarget::TextPrompt => {
                app.editor_handler
                    .on_key_event(key, &mut app.text_prompt.editor);
            }
            EditorTarget::Message => {
                app.editor_handler
                    .on_key_event(key, &mut app.messages_panel.editor);
            }
        }
    }
}

/// Scroll an editor down by moving the cursor down `n` lines.
fn scroll_editor_down(app: &mut VizApp, n: usize, target: EditorTarget) {
    use crossterm::event::KeyEvent;
    for _ in 0..n {
        let key = KeyEvent::new(KeyCode::Down, crossterm::event::KeyModifiers::NONE);
        match target {
            EditorTarget::Chat => {
                app.editor_handler.on_key_event(key, &mut app.chat.editor);
            }
            EditorTarget::TextPrompt => {
                app.editor_handler
                    .on_key_event(key, &mut app.text_prompt.editor);
            }
            EditorTarget::Message => {
                app.editor_handler
                    .on_key_event(key, &mut app.messages_panel.editor);
            }
        }
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

/// Jump the graph pane vertical scroll to a position proportional to `row` within the scrollbar.
fn vscrollbar_jump_graph(app: &mut VizApp, row: u16) {
    let sb = app.last_graph_scrollbar_area;
    if sb.height == 0 {
        return;
    }
    let max_offset = app
        .scroll
        .content_height
        .saturating_sub(app.scroll.viewport_height);
    if max_offset == 0 {
        return;
    }
    let row_in_track = row.saturating_sub(sb.y) as usize;
    let track_height = sb.height as usize;
    // Inverse of ratatui's thumb positioning: thumb_start = pos * track / max_viewport_pos,
    // where max_viewport_pos = content_length - 1 + viewport_length = max_offset - 1 + track.
    let new_offset = if track_height == 0 {
        0
    } else {
        let max_vp = max_offset.saturating_sub(1) + track_height;
        (row_in_track * max_vp) / track_height
    };
    app.scroll.offset_y = new_offset.min(max_offset);
}

/// Jump the right panel vertical scroll to a position proportional to `row` within the scrollbar.
fn vscrollbar_jump_panel(app: &mut VizApp, row: u16) {
    let sb = app.last_panel_scrollbar_area;
    if sb.height == 0 {
        return;
    }
    let row_in_track = row.saturating_sub(sb.y) as usize;
    let track_height = sb.height as usize;

    // Helper: inverse of ratatui's thumb positioning formula.
    // thumb_start = pos * track / (content_length - 1 + viewport_length).
    // Since content_length = max_scroll and viewport_length = track_height:
    //   max_viewport_pos = max_scroll - 1 + track_height.
    let jump = |max_scroll: usize| -> usize {
        if track_height == 0 {
            return 0;
        }
        let max_vp = max_scroll.saturating_sub(1) + track_height;
        ((row_in_track * max_vp) / track_height).min(max_scroll)
    };

    match app.right_panel_tab {
        RightPanelTab::Detail => {
            let max_scroll = app
                .hud_wrapped_line_count
                .saturating_sub(app.hud_detail_viewport_height);
            if max_scroll == 0 {
                return;
            }
            app.hud_scroll = jump(max_scroll);
        }
        RightPanelTab::Chat => {
            // Chat scroll is inverted: 0 = bottom, higher = further from bottom.
            let total = app.chat.total_rendered_lines;
            let viewport = app.chat.viewport_height;
            let max_scroll = total.saturating_sub(viewport);
            if max_scroll == 0 {
                return;
            }
            let scroll_from_top = jump(max_scroll);
            // Convert scroll_from_top to chat's inverted scroll.
            app.chat.scroll = max_scroll.saturating_sub(scroll_from_top);
        }
        RightPanelTab::Log => {
            let total = app.log_pane.total_wrapped_lines;
            let viewport = app.log_pane.viewport_height;
            let max_scroll = total.saturating_sub(viewport);
            if max_scroll == 0 {
                return;
            }
            app.log_pane.scroll = jump(max_scroll);
        }
        RightPanelTab::Messages => {
            let total = app.messages_panel.total_wrapped_lines;
            let viewport = app.messages_panel.viewport_height;
            let max_scroll = total.saturating_sub(viewport);
            if max_scroll == 0 {
                return;
            }
            app.messages_panel.scroll = jump(max_scroll);
        }
        RightPanelTab::Agency => {
            let total = app.agent_monitor.total_rendered_lines;
            let viewport = app.agent_monitor.viewport_height;
            let max_scroll = total.saturating_sub(viewport);
            if max_scroll == 0 {
                return;
            }
            app.agent_monitor.scroll = jump(max_scroll);
        }
        RightPanelTab::CoordLog => {
            let total = app.coord_log.total_wrapped_lines;
            let viewport = app.coord_log.viewport_height;
            let max_scroll = total.saturating_sub(viewport);
            if max_scroll == 0 {
                return;
            }
            app.coord_log.scroll = jump(max_scroll);
        }
        RightPanelTab::Firehose => {
            let total = app.firehose.total_rendered_lines;
            let viewport = app.firehose.viewport_height;
            let max_scroll = total.saturating_sub(viewport);
            if max_scroll == 0 {
                return;
            }
            let new_scroll = jump(max_scroll);
            app.firehose.scroll = new_scroll;
            app.firehose.auto_tail = new_scroll >= max_scroll;
        }
        _ => {}
    }
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

    // Pre-extract attach path before the match to avoid borrow conflict with app.attach_file().
    let attach_path = if code == KeyCode::Char('a') && fb.focus == FileBrowserFocus::Tree {
        fb.selected_path().filter(|p| p.is_file())
    } else {
        None
    };

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
            // 'a': attach the selected file (handled after match)
            KeyCode::Char('a') => {}
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

    // Attach file after the match block (fb borrow is dropped).
    if let Some(path) = attach_path {
        app.attach_file(&path.to_string_lossy());
    }
}

/// Determine which tab was clicked based on column position within the tab bar.
/// Returns None if the click is on a divider or beyond the last tab.
fn tab_at_column(col: u16) -> Option<RightPanelTab> {
    let labels = [
        "0:Chat", "1:Detail", "2:Log", "3:Msg", "4:Agency", "5:Config", "6:Files", "7:Coord",
        "8:Fire",
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

// ══════════════════════════════════════════════════════════════════════════════
// Tests for scrollbar click and drag behavior
// ══════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod scrollbar_tests {
    use super::*;
    use crate::commands::viz::LayoutMode as VizLayoutMode;
    use crate::commands::viz::ascii::generate_ascii;
    use crate::tui::viz_viewer::state::ScrollbarDragTarget;
    use ratatui::layout::Rect;
    use std::collections::{HashMap, HashSet};
    use workgraph::graph::{Node, Status, WorkGraph};
    use workgraph::parser::save_graph;
    use workgraph::test_helpers::make_task_with_status;

    /// Build a minimal graph and VizApp for scrollbar testing.
    /// Returns (VizApp, TempDir) — keep TempDir alive.
    fn build_test_app() -> (VizApp, tempfile::TempDir) {
        let mut graph = WorkGraph::new();
        // Create enough tasks that scrolling makes sense.
        for i in 0..20 {
            let id = format!("task-{}", i);
            let title = format!("Task {}", i);
            let t = make_task_with_status(&id, &title, Status::Open);
            graph.add_node(Node::Task(t));
        }

        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph.jsonl");
        save_graph(&graph, &graph_path).unwrap();

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            VizLayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
        );

        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.workgraph_dir = tmp.path().to_path_buf();
        (app, tmp)
    }

    /// Configure the app's graph scroll state so that scrollbar interactions
    /// have meaningful content to scroll through.
    fn setup_graph_scroll(app: &mut VizApp, content_height: usize, viewport_height: usize) {
        app.scroll.content_height = content_height;
        app.scroll.viewport_height = viewport_height;
        app.scroll.offset_y = 0;
    }

    // ── 1. Scrollbar hit-testing ──

    #[test]
    fn click_inside_graph_scrollbar_detected() {
        let (mut app, _tmp) = build_test_app();
        // Scrollbar occupies rightmost column of graph area.
        // Place scrollbar area at x=79, y=1, height=20 (typical right edge).
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 1,
            width: 1,
            height: 20,
        };
        let pos = Position::new(79, 10); // Inside scrollbar
        assert!(
            app.last_graph_scrollbar_area.height > 0 && app.last_graph_scrollbar_area.contains(pos),
            "Click at (79,10) should be inside graph scrollbar"
        );
    }

    #[test]
    fn click_outside_graph_scrollbar_not_detected() {
        let (mut app, _tmp) = build_test_app();
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 1,
            width: 1,
            height: 20,
        };
        let pos = Position::new(78, 10); // One column to the left
        assert!(
            !app.last_graph_scrollbar_area.contains(pos),
            "Click at (78,10) should NOT be inside scrollbar at x=79"
        );
    }

    #[test]
    fn click_on_scrollbar_boundary_top() {
        let (mut app, _tmp) = build_test_app();
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 1,
            width: 1,
            height: 20,
        };
        // Top edge: (79, 1) — should be inside.
        let pos_top = Position::new(79, 1);
        assert!(app.last_graph_scrollbar_area.contains(pos_top));
        // Just above: (79, 0) — should be outside.
        let pos_above = Position::new(79, 0);
        assert!(!app.last_graph_scrollbar_area.contains(pos_above));
    }

    #[test]
    fn click_on_scrollbar_boundary_bottom() {
        let (mut app, _tmp) = build_test_app();
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 1,
            width: 1,
            height: 20,
        };
        // Bottom edge: (79, 20) — y=1+20-1=20, should be inside.
        let pos_bottom = Position::new(79, 20);
        assert!(app.last_graph_scrollbar_area.contains(pos_bottom));
        // Just below: (79, 21) — should be outside.
        let pos_below = Position::new(79, 21);
        assert!(!app.last_graph_scrollbar_area.contains(pos_below));
    }

    #[test]
    fn click_inside_panel_scrollbar_detected() {
        let (mut app, _tmp) = build_test_app();
        app.last_panel_scrollbar_area = Rect {
            x: 119,
            y: 1,
            width: 1,
            height: 30,
        };
        let pos = Position::new(119, 15);
        assert!(
            app.last_panel_scrollbar_area.height > 0 && app.last_panel_scrollbar_area.contains(pos)
        );
    }

    #[test]
    fn zero_height_scrollbar_never_hit() {
        let (mut app, _tmp) = build_test_app();
        app.last_graph_scrollbar_area = Rect::default(); // zero-size
        let pos = Position::new(0, 0);
        // Even if contains() might return true for (0,0) in a zero rect,
        // the code guards with height > 0.
        let detected =
            app.last_graph_scrollbar_area.height > 0 && app.last_graph_scrollbar_area.contains(pos);
        assert!(
            !detected,
            "Zero-height scrollbar should never register a hit"
        );
    }

    // ── 2. Proportional scroll calculation ──

    #[test]
    fn vscrollbar_jump_graph_midpoint() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20); // max_offset = 80
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        // Click at row 10 out of 20 (50% of track).
        // max_vp = 80 - 1 + 20 = 99, new_offset = (10 * 99) / 20 = 49
        vscrollbar_jump_graph(&mut app, 10);
        assert_eq!(app.scroll.offset_y, 49);
    }

    #[test]
    fn vscrollbar_jump_graph_top() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        // Click at row 0 (top of scrollbar).
        vscrollbar_jump_graph(&mut app, 0);
        assert_eq!(app.scroll.offset_y, 0);
    }

    #[test]
    fn vscrollbar_jump_graph_bottom() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        // Click at row 19 (bottom of scrollbar, 0-indexed within track).
        // new_offset = (19 * 80) / 19 = 80
        vscrollbar_jump_graph(&mut app, 19);
        assert_eq!(app.scroll.offset_y, 80);
    }

    #[test]
    fn vscrollbar_jump_graph_with_offset_scrollbar_area() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        // Scrollbar starts at y=5 (not y=0).
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 5,
            width: 1,
            height: 20,
        };
        // Click at absolute row 5 → row_in_track = 5 - 5 = 0 → offset 0.
        vscrollbar_jump_graph(&mut app, 5);
        assert_eq!(app.scroll.offset_y, 0);

        // Click at absolute row 15 → row_in_track = 15 - 5 = 10.
        // max_vp = 80 - 1 + 20 = 99, new_offset = (10 * 99) / 20 = 49
        vscrollbar_jump_graph(&mut app, 15);
        assert_eq!(app.scroll.offset_y, 49);
    }

    #[test]
    fn vscrollbar_jump_graph_no_scroll_needed() {
        let (mut app, _tmp) = build_test_app();
        // Content fits in viewport: no scrolling possible.
        setup_graph_scroll(&mut app, 10, 20);
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.scroll.offset_y = 0;
        vscrollbar_jump_graph(&mut app, 10);
        assert_eq!(
            app.scroll.offset_y, 0,
            "Should not scroll when content fits in viewport"
        );
    }

    #[test]
    fn vscrollbar_jump_graph_zero_height_scrollbar() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.last_graph_scrollbar_area = Rect::default(); // height=0
        app.scroll.offset_y = 5;
        vscrollbar_jump_graph(&mut app, 10);
        assert_eq!(
            app.scroll.offset_y, 5,
            "Should not change offset when scrollbar height is 0"
        );
    }

    #[test]
    fn vscrollbar_jump_panel_detail_tab() {
        let (mut app, _tmp) = build_test_app();
        app.right_panel_tab = RightPanelTab::Detail;
        app.hud_wrapped_line_count = 100;
        app.hud_detail_viewport_height = 20;
        app.last_panel_scrollbar_area = Rect {
            x: 119,
            y: 0,
            width: 1,
            height: 20,
        };
        // Click at 50%: row_in_track=10, max_scroll=80.
        // max_vp = 80 - 1 + 20 = 99, pos = (10 * 99) / 20 = 49
        vscrollbar_jump_panel(&mut app, 10);
        assert_eq!(app.hud_scroll, 49);
    }

    #[test]
    fn vscrollbar_jump_panel_no_scroll_content_fits() {
        let (mut app, _tmp) = build_test_app();
        app.right_panel_tab = RightPanelTab::Detail;
        app.hud_wrapped_line_count = 10;
        app.hud_detail_viewport_height = 20;
        app.last_panel_scrollbar_area = Rect {
            x: 119,
            y: 0,
            width: 1,
            height: 20,
        };
        app.hud_scroll = 0;
        vscrollbar_jump_panel(&mut app, 10);
        assert_eq!(app.hud_scroll, 0, "No scroll when content fits in viewport");
    }

    #[test]
    fn hscrollbar_jump_midpoint() {
        let (mut app, _tmp) = build_test_app();
        app.scroll.content_width = 200;
        app.scroll.viewport_width = 80;
        // max_offset = 120
        app.last_graph_hscrollbar_area = Rect {
            x: 0,
            y: 29,
            width: 80,
            height: 1,
        };
        // Click at column 40 (50% of 80-wide track).
        // col_in_track = 40, new_offset = (40 * 120) / 79 = 60
        hscrollbar_jump_to_column(&mut app, 40);
        assert_eq!(app.scroll.offset_x, (40 * 120) / 79);
    }

    #[test]
    fn hscrollbar_jump_no_scroll_needed() {
        let (mut app, _tmp) = build_test_app();
        app.scroll.content_width = 50;
        app.scroll.viewport_width = 80;
        app.last_graph_hscrollbar_area = Rect {
            x: 0,
            y: 29,
            width: 80,
            height: 1,
        };
        app.scroll.offset_x = 0;
        hscrollbar_jump_to_column(&mut app, 40);
        assert_eq!(app.scroll.offset_x, 0);
    }

    // ── 3. Drag state management ──

    #[test]
    fn mousedown_on_graph_scrollbar_starts_drag() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        // Ensure no panel scrollbar conflicts.
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        assert!(app.scrollbar_drag.is_none());
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 79);
        assert_eq!(app.scrollbar_drag, Some(ScrollbarDragTarget::Graph));
    }

    #[test]
    fn mousedown_on_panel_scrollbar_starts_drag() {
        let (mut app, _tmp) = build_test_app();
        app.right_panel_tab = RightPanelTab::Detail;
        app.hud_wrapped_line_count = 100;
        app.hud_detail_viewport_height = 20;
        app.last_panel_scrollbar_area = Rect {
            x: 119,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_graph_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        assert!(app.scrollbar_drag.is_none());
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 119);
        assert_eq!(app.scrollbar_drag, Some(ScrollbarDragTarget::Panel));
    }

    #[test]
    fn drag_updates_scroll_position() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Start drag.
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 79);
        assert_eq!(app.scroll.offset_y, 0);

        // Drag to midpoint.
        // max_vp = 80 - 1 + 20 = 99, offset = (10 * 99) / 20 = 49
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 10, 79);
        assert_eq!(app.scroll.offset_y, 49);

        // Drag to near bottom.
        // offset = (18 * 99) / 20 = 89, clamped to max_offset 80
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 18, 79);
        assert_eq!(app.scroll.offset_y, 80);
    }

    #[test]
    fn mouseup_clears_drag_state() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Start drag.
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 5, 79);
        assert!(app.scrollbar_drag.is_some());

        // Release.
        handle_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 5, 79);
        assert!(
            app.scrollbar_drag.is_none(),
            "Drag state should be cleared on mouse up"
        );
    }

    #[test]
    fn drag_without_prior_mousedown_has_no_effect() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();
        app.scroll.offset_y = 5;

        // Drag event without prior mousedown — scrollbar_drag is None.
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 15, 79);
        assert_eq!(
            app.scroll.offset_y, 5,
            "Drag without active drag state should not change scroll"
        );
    }

    // ── 4. Simulated mouse event sequences ──

    #[test]
    fn full_click_drag_release_sequence_graph() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Step 1: MouseDown at top of scrollbar.
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 79);
        assert_eq!(app.scrollbar_drag, Some(ScrollbarDragTarget::Graph));
        assert_eq!(app.scroll.offset_y, 0);

        // Step 2: Drag to row 5.
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 5, 79);
        let pos_at_5 = app.scroll.offset_y;
        assert!(pos_at_5 > 0, "Dragging down should increase scroll offset");

        // Step 3: Drag to row 15.
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 15, 79);
        let pos_at_15 = app.scroll.offset_y;
        assert!(
            pos_at_15 > pos_at_5,
            "Dragging further down should increase scroll more"
        );

        // Step 4: MouseUp.
        handle_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 15, 79);
        assert!(app.scrollbar_drag.is_none());
        // Scroll position preserved after release.
        assert_eq!(app.scroll.offset_y, pos_at_15);
    }

    #[test]
    fn full_click_drag_release_sequence_panel() {
        let (mut app, _tmp) = build_test_app();
        app.right_panel_tab = RightPanelTab::Detail;
        app.hud_wrapped_line_count = 100;
        app.hud_detail_viewport_height = 20;
        app.last_panel_scrollbar_area = Rect {
            x: 119,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_graph_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // MouseDown on panel scrollbar.
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 119);
        assert_eq!(app.scrollbar_drag, Some(ScrollbarDragTarget::Panel));
        assert_eq!(app.hud_scroll, 0);

        // Drag to row 10.
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 10, 119);
        let mid_scroll = app.hud_scroll;
        assert!(mid_scroll > 0);

        // Drag to row 19.
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 19, 119);
        assert_eq!(app.hud_scroll, 80); // max scroll

        // Release.
        handle_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 19, 119);
        assert!(app.scrollbar_drag.is_none());
        assert_eq!(app.hud_scroll, 80); // preserved
    }

    #[test]
    fn horizontal_scrollbar_click_drag_release() {
        let (mut app, _tmp) = build_test_app();
        app.scroll.content_width = 200;
        app.scroll.viewport_width = 80;
        app.last_graph_hscrollbar_area = Rect {
            x: 0,
            y: 29,
            width: 80,
            height: 1,
        };
        app.last_graph_scrollbar_area = Rect::default();
        app.last_panel_scrollbar_area = Rect::default();

        // MouseDown on horizontal scrollbar.
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 29, 0);
        assert_eq!(
            app.scrollbar_drag,
            Some(ScrollbarDragTarget::GraphHorizontal)
        );
        assert_eq!(app.scroll.offset_x, 0);

        // Drag to column 40.
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 29, 40);
        assert!(app.scroll.offset_x > 0);

        // Release.
        handle_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 29, 40);
        assert!(app.scrollbar_drag.is_none());
    }

    #[test]
    fn click_outside_scrollbar_does_not_start_drag() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();
        // Set graph area so the click registers as a graph click, not scrollbar.
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: 79,
            height: 20,
        };

        // Click inside graph area but NOT on scrollbar.
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 50);
        assert!(
            app.scrollbar_drag.is_none(),
            "Click inside graph body should not start scrollbar drag"
        );
    }

    #[test]
    fn drag_position_clamped_to_max() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20); // max_offset = 80
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Start drag.
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 79);

        // Drag way beyond the scrollbar bottom.
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 100, 79);
        assert!(
            app.scroll.offset_y <= 80,
            "Scroll position should be clamped to max_offset (80), got {}",
            app.scroll.offset_y
        );
    }

    // ── 5. Scrollbar visibility ──

    #[test]
    fn graph_scrollbar_visible_during_drag() {
        let (mut app, _tmp) = build_test_app();
        app.scrollbar_drag = Some(ScrollbarDragTarget::Graph);
        assert!(
            app.graph_scrollbar_visible(),
            "Scrollbar should be visible while dragging"
        );
    }

    #[test]
    fn panel_scrollbar_visible_during_drag() {
        let (mut app, _tmp) = build_test_app();
        app.scrollbar_drag = Some(ScrollbarDragTarget::Panel);
        assert!(
            app.panel_scrollbar_visible(),
            "Panel scrollbar should be visible while dragging"
        );
    }

    #[test]
    fn graph_scrollbar_not_visible_without_activity() {
        let (mut app, _tmp) = build_test_app();
        app.scrollbar_drag = None;
        app.graph_scroll_activity = None;
        assert!(
            !app.graph_scrollbar_visible(),
            "Scrollbar should not be visible without recent activity"
        );
    }

    #[test]
    fn scrollbar_visible_after_scroll_activity() {
        let (mut app, _tmp) = build_test_app();
        app.record_graph_scroll_activity();
        assert!(
            app.graph_scrollbar_visible(),
            "Scrollbar should be visible immediately after scroll activity"
        );
    }

    // ── 6. Touch drag-to-pan ──

    #[test]
    fn drag_in_graph_body_pans_vertically() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.scroll.content_width = 200;
        app.scroll.viewport_width = 80;
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: 79,
            height: 20,
        };
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Mouse down inside graph body.
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 40);
        assert!(
            app.graph_pan_last.is_some(),
            "Pan should start on mouse down in graph"
        );
        assert_eq!(app.scroll.offset_y, 0);

        // Drag upward (row decreases from 10 to 5) → content follows finger up → scroll_down.
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 5, 40);
        assert_eq!(
            app.scroll.offset_y, 5,
            "Dragging up should scroll down by 5 rows"
        );

        // Drag back down (row increases from 5 to 8) → content follows finger down → scroll_up.
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 8, 40);
        assert_eq!(
            app.scroll.offset_y, 2,
            "Dragging down should scroll up by 3 rows"
        );
    }

    #[test]
    fn drag_in_graph_body_pans_horizontally() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.scroll.content_width = 200;
        app.scroll.viewport_width = 80;
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: 79,
            height: 20,
        };
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Mouse down inside graph body.
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 40);
        assert_eq!(app.scroll.offset_x, 0);

        // Drag left (column decreases from 40 to 30) → content follows finger left → scroll_right.
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 10, 30);
        assert_eq!(
            app.scroll.offset_x, 10,
            "Dragging left should scroll right by 10 cols"
        );

        // Drag right (column increases from 30 to 35) → content follows finger right → scroll_left.
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 10, 35);
        assert_eq!(
            app.scroll.offset_x, 5,
            "Dragging right should scroll left by 5 cols"
        );
    }

    #[test]
    fn drag_pan_cleared_on_mouse_up() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: 79,
            height: 20,
        };
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 40);
        assert!(app.graph_pan_last.is_some());

        handle_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 5, 40);
        assert!(
            app.graph_pan_last.is_none(),
            "Pan state should be cleared on mouse up"
        );
    }

    #[test]
    fn drag_pan_does_not_conflict_with_scrollbar_drag() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.scroll.content_width = 200;
        app.scroll.viewport_width = 80;
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: 79,
            height: 20,
        };
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Click on scrollbar, not graph body.
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 79);
        assert_eq!(app.scrollbar_drag, Some(ScrollbarDragTarget::Graph));
        // graph_pan_last should NOT be set since this was a scrollbar click.
        assert!(
            app.graph_pan_last.is_none(),
            "Scrollbar click should not start graph pan"
        );
    }

    #[test]
    fn drag_pan_diagonal_movement() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.scroll.content_width = 200;
        app.scroll.viewport_width = 80;
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: 79,
            height: 20,
        };
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Mouse down at (row=10, col=40).
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 40);

        // Drag diagonally: row 10→5 (up 5), col 40→30 (left 10).
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 5, 30);
        assert_eq!(app.scroll.offset_y, 5, "Vertical pan from diagonal drag");
        assert_eq!(app.scroll.offset_x, 10, "Horizontal pan from diagonal drag");
    }

    #[test]
    fn mouse_wheel_scroll_still_works_with_pan_state() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: 79,
            height: 20,
        };
        app.last_graph_scrollbar_area = Rect::default();
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Scroll wheel should work normally.
        handle_mouse(&mut app, MouseEventKind::ScrollDown, 10, 40);
        assert_eq!(
            app.scroll.offset_y, 3,
            "Mouse wheel ScrollDown should scroll by 3"
        );

        handle_mouse(&mut app, MouseEventKind::ScrollUp, 10, 40);
        assert_eq!(
            app.scroll.offset_y, 0,
            "Mouse wheel ScrollUp should scroll back"
        );
    }
}
