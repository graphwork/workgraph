use std::io;
use std::sync::mpsc;

use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::DefaultTerminal;
use ratatui::layout::Position;

use super::render;

/// Minimum inspector panel percentage during divider drag.
/// Prevents collapsing the inspector to nothing — the panel always gets at
/// least this share of the space.  The user can still reach Off mode via
/// keyboard shortcuts (`=`, `\`).
const MIN_DRAG_PERCENT: i32 = 10;

use super::state::{
    ChoiceDialogAction, ChoiceDialogState, CommandEffect, ConfigEditKind, ConfirmAction,
    ControlPanelFocus, FocusedPanel, InputMode, InspectorSubFocus, NavEntry, ResponsiveBreakpoint,
    RightPanelTab, TabBarEntryKind, TaskFormField, TextPromptAction, VizApp,
};

/// Handle content reload when iteration changes.
fn handle_iteration_change(app: &mut VizApp) {
    // Always reload Detail tab content
    app.load_hud_detail();

    // Invalidate Log and Messages panes so they reload with updated headers
    app.invalidate_log_pane();
    app.invalidate_messages_panel();

    // Force reload of the current tab's content
    match app.right_panel_tab {
        RightPanelTab::Log => {
            app.load_log_pane();
        }
        RightPanelTab::Messages => {
            app.load_messages_panel();
        }
        _ => {} // Detail tab is already reloaded above
    }
}

/// Handle mouse clicks on the iteration navigator widget.
fn handle_iteration_navigator_click(app: &mut VizApp, click_column: u16) {
    if app.iteration_archives.is_empty() {
        return; // No iterations to navigate
    }

    let nav_area = app.last_iteration_nav_area;
    let relative_column = click_column.saturating_sub(nav_area.x);

    // Calculate click targets based on the navigator layout: "◀ iter 2/5 ▶"
    // Left arrow is at position 0, right arrow is at the end
    let total = app.iteration_archives.len() + 1;
    let current_display = match app.viewing_iteration {
        None => total,
        Some(idx) => idx + 1,
    };

    let navigator_text = format!("◀ iter {}/{} ▶", current_display, total);
    let text_width = navigator_text.chars().count() as u16;
    let right_arrow_pos = text_width.saturating_sub(1); // Position of ▶

    // Determine navigation capabilities
    let can_go_prev = match app.viewing_iteration {
        None => !app.iteration_archives.is_empty(),
        Some(idx) => idx > 0,
    };
    let can_go_next = match app.viewing_iteration {
        Some(idx) => idx + 1 < app.iteration_archives.len(),
        None => false,
    };

    if relative_column == 0 && can_go_prev {
        // Click on left arrow ◀
        if app.iteration_prev() {
            handle_iteration_change(app);
            let total = app.iteration_archives.len() + 1;
            let msg = match app.viewing_iteration {
                Some(idx) => format!("Viewing iteration {}/{}", idx + 1, total),
                None => format!("Viewing current ({}/{})", total, total),
            };
            app.push_toast(msg, super::state::ToastSeverity::Info);
        }
    } else if relative_column == right_arrow_pos && can_go_next {
        // Click on right arrow ▶
        if app.iteration_next() {
            handle_iteration_change(app);
            let total = app.iteration_archives.len() + 1;
            let msg = match app.viewing_iteration {
                Some(idx) => format!("Viewing iteration {}/{}", idx + 1, total),
                None => format!("Viewing current ({}/{})", total, total),
            };
            app.push_toast(msg, super::state::ToastSeverity::Info);
        }
    }
    // Clicks on the counter text (middle area) are ignored
}

/// Apply the current mouse capture state to the terminal.
///
/// Uses modes 1002 (button-event tracking) and 1006 (SGR extended coordinates)
/// instead of crossterm's EnableMouseCapture which also enables 1003 (any-event).
/// Mode 1003 breaks mosh compatibility because mosh disables earlier modes when
/// a new mode arrives — leaving no tracking mode active. Mode 1002 adds drag
/// reporting (motion while button held) on top of 1000 (button tracking), which
/// is needed for scrollbar dragging.
///
/// When `any_motion` is true (auto-set for Termux without mosh), mode 1003 is
/// also enabled so that all motion events are reported. This helps with touch
/// environments where drag events may lack the button-held flag.
fn set_mouse_capture(enabled: bool, any_motion: bool) -> Result<()> {
    use io::Write;
    let mut stdout = io::stdout();
    if enabled {
        stdout.write_all(b"\x1b[?1002h\x1b[?1006h")?;
        if any_motion {
            stdout.write_all(b"\x1b[?1003h")?;
        }
    } else {
        stdout.write_all(b"\x1b[?1003l\x1b[?1006l\x1b[?1002l")?;
    }
    stdout.flush()?;
    Ok(())
}

/// Returns true when mode 1003 (any-event tracking) should be enabled.
/// Only enabled in Termux (without mosh), where touch drag events may lack
/// the button-held flag. Enabling 1003 globally causes the outer terminal
/// to report all motion events, which breaks touch gesture translation in
/// terminal emulators like Termux when running inside tmux.
pub(super) fn detect_any_motion_support() -> bool {
    std::env::var_os("TERMUX_VERSION").is_some() && std::env::var_os("MOSH_SERVER_PID").is_none()
}

pub fn run_event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut VizApp,
    shared_screen: &super::screen_dump::SharedScreen,
) -> Result<()> {
    // Set initial mouse capture state
    set_mouse_capture(app.mouse_enabled, app.any_motion_mouse)?;

    let result = run_event_loop_inner(terminal, app, shared_screen);

    // Save all coordinator chat states and TUI focus state before exit.
    app.save_all_chat_state();

    // Always disable mouse capture on exit
    let _ = set_mouse_capture(false, false);

    result
}

fn run_event_loop_inner(
    terminal: &mut DefaultTerminal,
    app: &mut VizApp,
    shared_screen: &super::screen_dump::SharedScreen,
) -> Result<()> {
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

    // Always draw the first frame.
    let mut needs_redraw = true;

    loop {
        let refreshed = app.maybe_refresh();
        let drained = app.drain_commands();

        // Phase 3c takeover poll. When the user sent a message in
        // observer mode, we wrote a release marker and set
        // chat_pty_takeover_pending_since. Poll the session lock
        // each iteration: once the external handler releases (or
        // after 15s timeout), drop the observer pane and spawn a
        // fresh owner pane so the conversation continues live.
        let takeover_redraw = poll_chat_pty_takeover(app);

        if needs_redraw || refreshed || drained || takeover_redraw {
            let completed = terminal.draw(|frame| render::draw(frame, app))?;
            // Update the shared screen snapshot for IPC dump clients.
            update_shared_screen(completed.buffer, app, shared_screen);
            needs_redraw = false;
        }

        // Adaptive poll timeout: short during animations for smooth rendering,
        // longer when idle to reduce CPU usage (from ~50% to <5%).
        let poll_timeout = app.next_poll_timeout();

        // Wait for the first event (up to poll_timeout), then drain all
        // immediately queued events before redrawing — same batching
        // strategy as before, but via the channel instead of raw polling.
        match rx.recv_timeout(poll_timeout) {
            Ok(ev) => {
                dispatch_event(app, ev);
                needs_redraw = true;
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
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Timeout: redraw if timed UI elements changed (animation
                // progress, notification expiry, scrollbar fade) or if a
                // data refresh tick is due.
                if app.has_timed_ui_elements() || app.is_refresh_due() {
                    needs_redraw = true;
                }
                // Flush trace buffer during idle moments.
                if let Some(tracer) = app.tracer.as_mut() {
                    tracer.flush();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                anyhow::bail!("terminal event reader thread exited unexpectedly");
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

/// Copy the current terminal buffer into the shared screen snapshot for IPC.
fn update_shared_screen(
    buf: &ratatui::buffer::Buffer,
    app: &VizApp,
    shared_screen: &super::screen_dump::SharedScreen,
) {
    let active_tab = app.right_panel_tab.label();
    let focused = match app.focused_panel {
        super::state::FocusedPanel::Graph => "graph",
        super::state::FocusedPanel::RightPanel => "panel",
    };
    let selected = app
        .selected_task_idx
        .and_then(|idx| app.task_order.get(idx))
        .map(|s| s.as_str());
    let input_mode = format!("{:?}", app.input_mode);
    super::screen_dump::update_snapshot(
        shared_screen,
        buf,
        active_tab,
        focused,
        selected,
        &input_mode,
        app.active_coordinator_id,
    );
}

/// Route a single crossterm event to the appropriate handler.
pub fn dispatch_event(app: &mut VizApp, ev: Event) {
    // Record the event to the trace file (if tracing is enabled).
    if app.tracer.is_some() {
        let ctx = super::trace::capture_state_context(app);
        if let Some(tracer) = app.tracer.as_mut() {
            tracer.record(&ev, ctx);
        }
    }

    match ev {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            // Record key feedback for overlay before handling (so we capture all keys).
            if app.key_feedback_enabled {
                let label = key_label(key.code, key.modifiers);
                app.record_key_feedback(label);
            }
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

/// Produce a human-readable label for a key press (for the key feedback overlay).
fn key_label(code: KeyCode, modifiers: KeyModifiers) -> String {
    let mut parts = Vec::new();
    if modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl");
    }
    if modifiers.contains(KeyModifiers::ALT) {
        parts.push("Alt");
    }
    if modifiers.contains(KeyModifiers::SHIFT) {
        parts.push("Shift");
    }
    let key_name = match code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Char(c) => {
            if modifiers.contains(KeyModifiers::SHIFT) && c.is_ascii_alphabetic() {
                c.to_uppercase().to_string()
            } else {
                c.to_string()
            }
        }
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "Shift+Tab".to_string(),
        KeyCode::Backspace => "Bksp".to_string(),
        KeyCode::Delete => "Del".to_string(),
        KeyCode::Up => "\u{2191}".to_string(),    // ↑
        KeyCode::Down => "\u{2193}".to_string(),  // ↓
        KeyCode::Left => "\u{2190}".to_string(),  // ←
        KeyCode::Right => "\u{2192}".to_string(), // →
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PgUp".to_string(),
        KeyCode::PageDown => "PgDn".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        _ => format!("{code:?}"),
    };
    // For Shift+arrow style combos, the key name already includes "Shift" for BackTab,
    // so avoid duplicate "Shift" prefix.
    if code == KeyCode::BackTab {
        return key_name;
    }
    if parts.is_empty() {
        key_name
    } else {
        parts.push(&key_name);
        // Filter duplicate "Shift" for shifted characters.
        if modifiers.contains(KeyModifiers::SHIFT) && code != KeyCode::BackTab {
            // For plain chars, Shift is implied in the uppercase char itself
            if let KeyCode::Char(c) = code
                && c.is_ascii_alphabetic()
            {
                // Already uppercased — remove Shift prefix
                parts.retain(|p| *p != "Shift");
            }
        }
        parts.join("+")
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
        InputMode::ChatSearch => handle_chat_search_input(app, code, modifiers),
        InputMode::TaskForm => handle_task_form_input(app, code, modifiers),
        InputMode::Confirm(_) => handle_confirm_input(app, code),
        InputMode::TextPrompt(_) => handle_text_prompt_input(app, code, modifiers),
        InputMode::ChoiceDialog(_) => handle_choice_dialog_input(app, code),
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
        InputMode::ChatSearch => {
            let clean: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            app.chat.search.query.push_str(&clean);
            app.update_chat_search();
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

fn handle_chat_search_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    match code {
        KeyCode::Esc => {
            app.clear_chat_search();
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Enter => {
            // Accept search — keep highlights, return to normal mode.
            // n/N will still navigate matches.
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace | KeyCode::Delete => {
            app.chat.search.query.pop();
            app.update_chat_search();
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.chat.search.query.clear();
            app.update_chat_search();
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.clear_chat_search();
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Char('n') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.chat_search_next();
        }
        KeyCode::Char('p') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.chat_search_prev();
        }
        KeyCode::Tab => {
            app.chat_search_next();
        }
        KeyCode::BackTab => {
            app.chat_search_prev();
        }
        KeyCode::Char('a') if modifiers.contains(KeyModifiers::CONTROL) => {
            // Ctrl+A: search all history (load unloaded pages).
            app.chat_search_load_all_history();
        }
        KeyCode::Char(c) => {
            app.chat.search.query.push(c);
            app.update_chat_search();
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

fn handle_choice_dialog_input(app: &mut VizApp, code: KeyCode) {
    let state = match &app.input_mode {
        InputMode::ChoiceDialog(s) => s.clone(),
        _ => return,
    };

    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            if let InputMode::ChoiceDialog(ref mut s) = app.input_mode
                && s.selected > 0
            {
                s.selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let InputMode::ChoiceDialog(ref mut s) = app.input_mode
                && s.selected + 1 < s.options.len()
            {
                s.selected += 1;
            }
        }
        KeyCode::Enter => {
            execute_choice_dialog_option(app, &state.action, state.selected);
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Char(c) => {
            // Check if the char matches a hotkey
            if let Some(idx) = state.options.iter().position(|(h, _, _)| *h == c) {
                execute_choice_dialog_option(app, &state.action, idx);
                app.input_mode = InputMode::Normal;
            }
        }
        _ => {}
    }
}

fn execute_choice_dialog_option(app: &mut VizApp, action: &ChoiceDialogAction, idx: usize) {
    match action {
        ChoiceDialogAction::RemoveCoordinator(cid) => {
            let cid = *cid;
            match idx {
                0 => {
                    // Archive
                    app.exec_command(
                        vec![
                            "service".to_string(),
                            "archive-coordinator".to_string(),
                            cid.to_string(),
                        ],
                        CommandEffect::ArchiveCoordinator(cid),
                    );
                }
                1 => {
                    // Stop
                    app.exec_command(
                        vec![
                            "service".to_string(),
                            "stop-coordinator".to_string(),
                            cid.to_string(),
                        ],
                        CommandEffect::StopCoordinator(cid),
                    );
                }
                2 => {
                    // Abandon (existing delete behavior)
                    app.delete_coordinator(cid);
                }
                _ => {}
            }
        }
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
    // When AgentSlots is focused, +/- and left/right adjust the slot count
    if app.service_health.panel_focus == ControlPanelFocus::AgentSlots {
        match code {
            KeyCode::Char('+') | KeyCode::Char('=') | KeyCode::Right | KeyCode::Char('l') => {
                app.adjust_agent_slots(1);
                return;
            }
            KeyCode::Char('-') | KeyCode::Left | KeyCode::Char('h') => {
                app.adjust_agent_slots(-1);
                return;
            }
            // Fall through for navigation keys (up/down/esc/etc.)
            _ => {}
        }
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
        // CreateCoordinator accepts empty text (creates unnamed coordinator)
        if action == TextPromptAction::CreateCoordinator {
            let name = if text.trim().is_empty() {
                None
            } else {
                Some(text.trim().to_string())
            };
            app.create_coordinator(name);
            app.input_mode = InputMode::Normal;
            return;
        }
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
            TextPromptAction::CreateCoordinator => unreachable!("handled above"),
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
    let in_edit_mode = app.chat.editing_index.is_some();
    match code {
        KeyCode::Esc => {
            if in_edit_mode {
                app.cancel_chat_edit_mode();
            } else {
                app.input_mode = InputMode::Normal;
                app.chat_input_dismissed = true;
                app.inspector_sub_focus = InspectorSubFocus::ChatHistory;
            }
            return;
        }
        KeyCode::Enter
            if !modifiers.contains(KeyModifiers::SHIFT)
                && !modifiers.contains(KeyModifiers::ALT) =>
        {
            if in_edit_mode {
                app.commit_chat_edit();
            } else {
                let text = editor_text(&app.chat.editor);
                editor_clear(&mut app.chat.editor);
                if !text.trim().is_empty() {
                    app.send_chat_message(text);
                }
            }
            return;
        }
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            if app.chat.awaiting_response() {
                // Interrupt the running coordinator instead of clearing input
                app.interrupt_coordinator();
            } else if in_edit_mode {
                app.cancel_chat_edit_mode();
            } else {
                editor_clear(&mut app.chat.editor);
                app.input_mode = InputMode::Normal;
                app.inspector_sub_focus = InspectorSubFocus::ChatHistory;
            }
            return;
        }
        KeyCode::Char('v') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.try_paste_clipboard_image();
            return;
        }
        KeyCode::Up
            if !modifiers.contains(KeyModifiers::ALT)
                && !modifiers.contains(KeyModifiers::SHIFT) =>
        {
            // Up arrow: navigate to previous user message (history)
            // Only trigger when input is empty (for fresh history nav) or already in history mode
            let is_empty = editor_text(&app.chat.editor).is_empty();
            if (is_empty || app.chat.history_cursor.is_some()) && app.chat_history_up() {
                return;
            }
        }
        KeyCode::Down
            if !modifiers.contains(KeyModifiers::ALT)
                && !modifiers.contains(KeyModifiers::SHIFT) =>
        {
            // Down arrow: navigate to next user message or back to fresh input
            if app.chat.history_cursor.is_some() && app.chat_history_down() {
                return;
            }
        }
        KeyCode::Up if modifiers.contains(KeyModifiers::ALT) => {
            app.record_panel_scroll_activity();
            app.chat.scroll = app.chat.scroll.saturating_add(1);
            maybe_load_more_chat_history(app);
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
    // Global Ctrl+T: toggle PTY-backed rendering for the Chat tab,
    // regardless of focused panel. Works from the graph pane or the
    // right panel — users expect "enable live terminal view" to be
    // always accessible. No-op when the active right tab isn't Chat.
    if modifiers.contains(KeyModifiers::CONTROL)
        && matches!(code, KeyCode::Char('t'))
        && app.right_panel_tab == RightPanelTab::Chat
    {
        toggle_chat_pty_mode(app);
        return;
    }
    match app.focused_panel {
        FocusedPanel::Graph => handle_graph_key(app, code, modifiers),
        FocusedPanel::RightPanel => handle_right_panel_key(app, code, modifiers),
    }
}

fn handle_graph_key(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    // When the history browser is active, intercept keys for history navigation.
    if app.history_browser.active {
        handle_history_browser_key(app, code, modifiers);
        return;
    }

    // When the archive browser is active, intercept keys for archive navigation.
    if app.archive_browser.active {
        handle_archive_key(app, code, modifiers);
        return;
    }

    match code {
        // Help overlay
        KeyCode::Char('?') => app.show_help = true,

        // Quit
        KeyCode::Char('q') => app.should_quit = true,
        KeyCode::Esc => {
            if app.has_active_search() {
                app.clear_search();
            } else if app.dismiss_error_toasts() {
                // Dismissed error toasts — don't quit.
            } else {
                app.should_quit = true;
            }
        }
        // Ctrl+C: interrupt coordinator if awaiting response in chat tab,
        // otherwise kill the agent on the focused task.
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            if app.chat.awaiting_response() && app.right_panel_tab == RightPanelTab::Chat {
                app.interrupt_coordinator();
            } else {
                app.kill_focused_agent();
            }
        }

        // Ctrl+H: open history browser
        KeyCode::Char('h') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.open_history_browser();
        }

        // Ctrl+R: quick resume when paused due to provider errors
        KeyCode::Char('r') if modifiers.contains(KeyModifiers::CONTROL) => {
            if app.service_health.paused && app.service_health.provider_auto_pause {
                app.exec_command(
                    vec!["service".into(), "resume".into()],
                    CommandEffect::RefreshAndNotify("Service resumed".into()),
                );
                app.push_toast(
                    "Service resumed from provider error".into(),
                    super::state::ToastSeverity::Info,
                );
            }
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

        // ]/[: cycle single-panel views in compact mode
        KeyCode::Char(']') if app.responsive_breakpoint == ResponsiveBreakpoint::Compact => {
            app.toggle_single_panel_view();
        }
        KeyCode::Char('[') if app.responsive_breakpoint == ResponsiveBreakpoint::Compact => {
            app.prev_single_panel_view();
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

        // Shift+Period (<): toggle showing only running system tasks
        KeyCode::Char('<') => {
            app.show_running_system_tasks = !app.show_running_system_tasks;
            app.system_tasks_just_toggled = true;
            app.force_refresh();
        }

        // *: toggle touch echo (click/touch visual feedback)
        KeyCode::Char('*') => {
            app.touch_echo_enabled = !app.touch_echo_enabled;
            if !app.touch_echo_enabled {
                app.touch_echoes.clear();
            }
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
        KeyCode::Char('v') => {
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
            let _ = set_mouse_capture(app.mouse_enabled, app.any_motion_mouse);
        }

        // Toggle scroll axis swap (vertical scroll ↔ horizontal scroll in graph)
        KeyCode::Char('X') => {
            app.scroll_axis_swapped = !app.scroll_axis_swapped;
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

        // A: toggle archive browser
        KeyCode::Char('A') => {
            app.toggle_archive_browser();
        }

        // Enter: open detail view for the selected task.
        // Agency tasks (.evaluate-*, .assign-*, .place-*, .flip-*, .create-*) drill
        // through to fullscreen detail so their logs/scores are immediately visible.
        KeyCode::Enter => {
            if let Some(task_id) = app.selected_task_id().map(|s| s.to_string()) {
                app.load_hud_detail_for_task(&task_id);
                app.right_panel_visible = true;
                app.right_panel_tab = RightPanelTab::Detail;
                if is_agency_task_id(&task_id) {
                    app.apply_layout_mode(super::state::LayoutMode::FullInspector);
                } else {
                    app.focused_panel = FocusedPanel::RightPanel;
                }
            }
        }

        // Digit keys 0-9: switch right panel tab
        KeyCode::Char(d @ '0'..='9') => {
            let idx = (d as u8 - b'0') as usize;
            if let Some(tab) = RightPanelTab::from_index(idx) {
                // Special behavior for '2' key (Log tab): toggle view mode if already active
                if d == '2' && app.right_panel_visible && app.right_panel_tab == RightPanelTab::Log
                {
                    app.toggle_log_view();
                } else {
                    app.right_panel_visible = true;
                    app.right_panel_tab = tab;
                }
            }
        }

        _ => {}
    }
}

fn handle_archive_key(app: &mut VizApp, code: KeyCode, _modifiers: KeyModifiers) {
    if app.archive_browser.filter_active {
        // Filter input mode: typing characters into the filter
        match code {
            KeyCode::Esc => {
                app.archive_browser.filter_active = false;
                app.archive_browser.filter.clear();
                app.archive_browser.apply_filter();
            }
            KeyCode::Enter => {
                app.archive_browser.filter_active = false;
            }
            KeyCode::Backspace => {
                app.archive_browser.filter.pop();
                app.archive_browser.apply_filter();
            }
            KeyCode::Char(c) => {
                app.archive_browser.filter.push(c);
                app.archive_browser.apply_filter();
            }
            _ => {}
        }
        return;
    }

    match code {
        // Close archive browser
        KeyCode::Esc | KeyCode::Char('A') => {
            app.archive_browser.active = false;
            app.archive_browser.filter_active = false;
        }
        KeyCode::Char('q') => {
            app.archive_browser.active = false;
            app.archive_browser.filter_active = false;
        }

        // Navigation
        KeyCode::Up | KeyCode::Char('k') => {
            if app.archive_browser.selected > 0 {
                app.archive_browser.selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let count = app.archive_browser.visible_count();
            if count > 0 && app.archive_browser.selected < count - 1 {
                app.archive_browser.selected += 1;
            }
        }
        KeyCode::Home | KeyCode::Char('g') => {
            app.archive_browser.selected = 0;
            app.archive_browser.scroll = 0;
        }
        KeyCode::End | KeyCode::Char('G') => {
            let count = app.archive_browser.visible_count();
            if count > 0 {
                app.archive_browser.selected = count - 1;
            }
        }
        KeyCode::PageUp => {
            app.archive_browser.selected = app.archive_browser.selected.saturating_sub(20);
        }
        KeyCode::PageDown => {
            let count = app.archive_browser.visible_count();
            if count > 0 {
                app.archive_browser.selected = (app.archive_browser.selected + 20).min(count - 1);
            }
        }

        // Search/filter
        KeyCode::Char('/') => {
            app.archive_browser.filter.clear();
            app.archive_browser.filter_active = true;
        }

        // Restore selected task
        KeyCode::Char('r') => {
            app.restore_archive_entry();
            // Reload after restore
            let dir = app.workgraph_dir.clone();
            app.archive_browser.load(&dir);
        }

        // Refresh archive list
        KeyCode::Char('R') => {
            let dir = app.workgraph_dir.clone();
            app.archive_browser.load(&dir);
        }

        _ => {}
    }
}

fn handle_history_browser_key(app: &mut VizApp, code: KeyCode, _modifiers: KeyModifiers) {
    if app.history_browser.preview_expanded {
        // Preview mode: scrolling through full content of selected segment
        match code {
            KeyCode::Esc | KeyCode::Char('q') => {
                app.history_browser.preview_expanded = false;
                app.history_browser.preview_scroll = 0;
            }
            KeyCode::Enter => {
                // Inject and close
                app.inject_selected_history();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.history_browser.preview_scroll =
                    app.history_browser.preview_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                app.history_browser.preview_scroll =
                    app.history_browser.preview_scroll.saturating_add(1);
            }
            KeyCode::PageUp => {
                app.history_browser.preview_scroll =
                    app.history_browser.preview_scroll.saturating_sub(20);
            }
            KeyCode::PageDown => {
                app.history_browser.preview_scroll =
                    app.history_browser.preview_scroll.saturating_add(20);
            }
            _ => {}
        }
        return;
    }

    match code {
        // Close history browser
        KeyCode::Esc | KeyCode::Char('q') => {
            app.close_history_browser();
        }

        // Navigation
        KeyCode::Up | KeyCode::Char('k') => {
            if app.history_browser.selected > 0 {
                app.history_browser.selected -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let count = app.history_browser.segments.len();
            if count > 0 && app.history_browser.selected < count - 1 {
                app.history_browser.selected += 1;
            }
        }
        KeyCode::Home | KeyCode::Char('g') => {
            app.history_browser.selected = 0;
            app.history_browser.scroll = 0;
        }
        KeyCode::End | KeyCode::Char('G') => {
            let count = app.history_browser.segments.len();
            if count > 0 {
                app.history_browser.selected = count - 1;
            }
        }

        // Enter: inject selected segment
        KeyCode::Enter => {
            app.inject_selected_history();
        }

        // Space: toggle preview expansion
        KeyCode::Char(' ') => {
            if !app.history_browser.segments.is_empty() {
                app.history_browser.preview_expanded = true;
                app.history_browser.preview_scroll = 0;
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
                KeyCode::Char('v') => app.shrink_viz_pane(),
                KeyCode::Esc => {
                    app.focused_panel = FocusedPanel::Graph;
                }
                KeyCode::Char(d @ '0'..='9') => {
                    let idx = (d as u8 - b'0') as usize;
                    if let Some(tab) = RightPanelTab::from_index(idx) {
                        // Special behavior for '2' key (Log tab): toggle view mode if already active
                        if d == '2' && app.right_panel_tab == RightPanelTab::Log {
                            app.toggle_log_view();
                        } else {
                            app.right_panel_tab = tab;
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
        // Ctrl+C: interrupt coordinator if awaiting response, else kill focused agent
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
            if app.chat.awaiting_response() && app.right_panel_tab == RightPanelTab::Chat {
                app.interrupt_coordinator();
            } else {
                app.kill_focused_agent();
            }
        }
        // Ctrl+H: open history browser
        KeyCode::Char('h') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.open_history_browser();
        }

        // Tab: switch panel focus back to graph
        KeyCode::Tab => {
            app.toggle_panel_focus();
        }

        // ]/[: cycle single-panel views in compact mode
        KeyCode::Char(']') if app.responsive_breakpoint == ResponsiveBreakpoint::Compact => {
            app.toggle_single_panel_view();
        }
        KeyCode::Char('[') if app.responsive_breakpoint == ResponsiveBreakpoint::Compact => {
            app.prev_single_panel_view();
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
        KeyCode::Char('v') => {
            app.shrink_viz_pane();
        }

        // Esc: clear chat search results first, then pop nav stack, otherwise go back to graph focus
        KeyCode::Esc => {
            if !app.chat.search.query.is_empty() && app.right_panel_tab == RightPanelTab::Chat {
                app.clear_chat_search();
            } else {
                nav_stack_pop(app);
            }
        }

        // Number keys 0-9 switch tabs (clears nav stack — manual navigation)
        KeyCode::Char(d @ '0'..='9') => {
            app.nav_stack.clear();
            let idx = (d as u8 - b'0') as usize;
            if let Some(tab) = RightPanelTab::from_index(idx) {
                // Special behavior for '2' key (Log tab): toggle view mode if already active
                if d == '2' && app.right_panel_tab == RightPanelTab::Log {
                    app.toggle_log_view();
                } else {
                    app.right_panel_tab = tab;
                }
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

        // Left/Right: on Chat tab, cycle coordinators; on Output tab, cycle agents; otherwise cycle tabs
        KeyCode::Left => {
            if app.right_panel_tab == RightPanelTab::Chat {
                let ids = app.list_coordinator_ids();
                if ids.len() > 1 {
                    let pos = ids
                        .iter()
                        .position(|&id| id == app.active_coordinator_id)
                        .unwrap_or(0);
                    let prev = if pos == 0 { ids.len() - 1 } else { pos - 1 };
                    app.switch_coordinator(ids[prev]);
                }
            } else if app.right_panel_tab == RightPanelTab::Output {
                let ids = app.output_pane_agent_ids();
                if ids.len() > 1 {
                    let pos = ids
                        .iter()
                        .position(|id| Some(id) == app.output_pane.active_agent_id.as_ref())
                        .unwrap_or(0);
                    let prev = if pos == 0 { ids.len() - 1 } else { pos - 1 };
                    app.output_pane.active_agent_id = Some(ids[prev].clone());
                    app.output_pane.has_new_content = false;
                }
            } else {
                app.nav_stack.clear();
                app.right_panel_tab = app.right_panel_tab.prev();
            }
        }
        KeyCode::Right => {
            if app.right_panel_tab == RightPanelTab::Chat {
                let ids = app.list_coordinator_ids();
                if ids.len() > 1 {
                    let pos = ids
                        .iter()
                        .position(|&id| id == app.active_coordinator_id)
                        .unwrap_or(0);
                    let next = (pos + 1) % ids.len();
                    app.switch_coordinator(ids[next]);
                }
            } else if app.right_panel_tab == RightPanelTab::Output {
                let ids = app.output_pane_agent_ids();
                if ids.len() > 1 {
                    let pos = ids
                        .iter()
                        .position(|id| Some(id) == app.output_pane.active_agent_id.as_ref())
                        .unwrap_or(0);
                    let next = (pos + 1) % ids.len();
                    app.output_pane.active_agent_id = Some(ids[next].clone());
                    app.output_pane.has_new_content = false;
                }
            } else {
                app.nav_stack.clear();
                app.right_panel_tab = app.right_panel_tab.next();
            }
        }

        // Dashboard: 'k' kills the selected agent instead of scrolling
        KeyCode::Char('k') if app.right_panel_tab == RightPanelTab::Dashboard => {
            if let Some(row) = app.dashboard.agent_rows.get(app.dashboard.selected_row) {
                let agent_id = row.agent_id.clone();
                let wg_dir = app.workgraph_dir.clone();
                let _ = std::process::Command::new("wg")
                    .arg("kill")
                    .arg(&agent_id)
                    .current_dir(&wg_dir)
                    .output();
                app.load_agent_monitor();
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
        // in config tab, start editing the selected setting; in log tab, toggle section.
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
            } else if app.right_panel_tab == RightPanelTab::Dashboard {
                // Drill-down: push Dashboard onto nav stack, switch to Output for selected agent
                if let Some(row) = app.dashboard.agent_rows.get(app.dashboard.selected_row) {
                    let agent_id = row.agent_id.clone();
                    app.nav_stack.push(NavEntry::Dashboard);
                    app.output_pane.active_agent_id = Some(agent_id);
                    app.right_panel_tab = RightPanelTab::Output;
                }
            } else if app.right_panel_tab == RightPanelTab::Output && !app.nav_stack.is_empty() {
                // Drill-down from Output: push AgentDetail, go to task Detail
                if let Some(ref agent_id) = app.output_pane.active_agent_id.clone() {
                    let task_id = app
                        .dashboard
                        .agent_rows
                        .iter()
                        .find(|r| r.agent_id == *agent_id)
                        .map(|r| r.task_id.clone());
                    if let Some(task_id) = task_id {
                        app.nav_stack.push(NavEntry::AgentDetail {
                            agent_id: agent_id.clone(),
                        });
                        app.load_hud_detail_for_task(&task_id);
                        app.right_panel_tab = RightPanelTab::Detail;
                    }
                }
            } else if app.right_panel_tab == RightPanelTab::Detail && !app.nav_stack.is_empty() {
                // Drill-down from Detail: push TaskDetail, go to Log tab
                if let Some(ref detail) = app.hud_detail {
                    let task_id = detail.task_id.clone();
                    app.nav_stack.push(NavEntry::TaskDetail {
                        task_id: task_id.clone(),
                    });
                    if let Some(idx) = app.task_order.iter().position(|id| *id == task_id) {
                        app.selected_task_idx = Some(idx);
                    }
                    app.invalidate_log_pane();
                    app.load_log_pane();
                    app.right_panel_tab = RightPanelTab::Log;
                }
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

        // Config tab: 'g' installs project config as global default
        KeyCode::Char('g') if app.right_panel_tab == RightPanelTab::Config => {
            app.install_config_as_global();
        }

        // Config tab: 't' tests the selected endpoint's connectivity
        KeyCode::Char('t') if app.right_panel_tab == RightPanelTab::Config => {
            app.test_selected_endpoint();
        }

        // Config tab: 'a' starts the add-endpoint flow
        KeyCode::Char('a') if app.right_panel_tab == RightPanelTab::Config => {
            app.config_panel.adding_endpoint = true;
            app.config_panel.new_endpoint = super::state::NewEndpointFields::default();
            app.config_panel.new_endpoint_field = 0;
            app.config_panel.editing = false;
            app.input_mode = InputMode::ConfigEdit;
        }

        // Config tab: 'm' starts the add-model flow
        KeyCode::Char('m') if app.right_panel_tab == RightPanelTab::Config => {
            app.config_panel.adding_model = true;
            app.config_panel.new_model = super::state::NewModelFields::default();
            app.config_panel.new_model_field = 0;
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

        // Dashboard tab: t = task detail, b = back
        KeyCode::Char('t') if app.right_panel_tab == RightPanelTab::Dashboard => {
            // Jump to task detail for the selected agent's task (push Dashboard onto nav stack)
            if let Some(row) = app.dashboard.agent_rows.get(app.dashboard.selected_row) {
                let task_id = row.task_id.clone();
                app.nav_stack.push(NavEntry::Dashboard);
                app.load_hud_detail_for_task(&task_id);
                app.right_panel_tab = RightPanelTab::Detail;
            }
        }
        KeyCode::Char('b')
            if app.right_panel_tab == RightPanelTab::Dashboard
                || app.right_panel_tab == RightPanelTab::Output
                || app.right_panel_tab == RightPanelTab::Detail
                || app.right_panel_tab == RightPanelTab::Log =>
        {
            // Back: pop nav stack if non-empty, otherwise return to graph focus
            nav_stack_pop(app);
        }

        // Chat tab: '[' / ']' cycle between coordinator tabs
        // Chat tab: Ctrl+T toggles PTY-backed rendering for the active
        // coordinator's task. Lazy-spawns `wg spawn-task <task-id>`
        // on first toggle-on; tears down cleanly on toggle-off or
        // when the embedded handler exits. Phase 3a of
        // docs/design/sessions-as-identity-rollout.md.
        KeyCode::Char('t')
            if modifiers.contains(KeyModifiers::CONTROL)
                && app.right_panel_tab == RightPanelTab::Chat =>
        {
            toggle_chat_pty_mode(app);
        }
        // Chat tab: when PTY mode is on and user is NOT in the chat
        // input editor, forward keys to the embedded handler's stdin.
        // Text input flows into rustyline/slash commands/etc. inside
        // the PTY.
        _ if app.chat_pty_mode
            && app.right_panel_tab == RightPanelTab::Chat
            && app.input_mode == InputMode::Normal =>
        {
            let task_id = format!(".coordinator-{}", app.active_coordinator_id);
            if let Some(pane) = app.task_panes.get_mut(&task_id) {
                let key = crossterm::event::KeyEvent::new(code, modifiers);
                let _ = pane.send_key(key);
            }
        }
        KeyCode::Char('[') if app.right_panel_tab == RightPanelTab::Chat => {
            let ids = app.list_coordinator_ids();
            if ids.len() > 1 {
                let pos = ids
                    .iter()
                    .position(|&id| id == app.active_coordinator_id)
                    .unwrap_or(0);
                let prev = if pos == 0 { ids.len() - 1 } else { pos - 1 };
                app.switch_coordinator(ids[prev]);
            }
        }
        KeyCode::Char(']') if app.right_panel_tab == RightPanelTab::Chat => {
            let ids = app.list_coordinator_ids();
            if ids.len() > 1 {
                let pos = ids
                    .iter()
                    .position(|&id| id == app.active_coordinator_id)
                    .unwrap_or(0);
                let next = (pos + 1) % ids.len();
                app.switch_coordinator(ids[next]);
            }
        }
        // Output tab: '[' switches to previous agent
        KeyCode::Char('[') if app.right_panel_tab == RightPanelTab::Output => {
            let ids = app.output_pane_agent_ids();
            if ids.len() > 1 {
                let pos = ids
                    .iter()
                    .position(|id| Some(id) == app.output_pane.active_agent_id.as_ref())
                    .unwrap_or(0);
                let prev = if pos == 0 { ids.len() - 1 } else { pos - 1 };
                app.output_pane.active_agent_id = Some(ids[prev].clone());
                app.output_pane.has_new_content = false;
            }
        }
        // Output tab: ']' switches to next agent
        KeyCode::Char(']') if app.right_panel_tab == RightPanelTab::Output => {
            let ids = app.output_pane_agent_ids();
            if ids.len() > 1 {
                let pos = ids
                    .iter()
                    .position(|id| Some(id) == app.output_pane.active_agent_id.as_ref())
                    .unwrap_or(0);
                let next = (pos + 1) % ids.len();
                app.output_pane.active_agent_id = Some(ids[next].clone());
                app.output_pane.has_new_content = false;
            }
        }
        // Chat tab: '+' creates a new coordinator session
        KeyCode::Char('+') if app.right_panel_tab == RightPanelTab::Chat => {
            super::state::editor_clear(&mut app.text_prompt.editor);
            app.input_mode = InputMode::TextPrompt(TextPromptAction::CreateCoordinator);
        }
        // Chat tab: '-' opens choice dialog for coordinator removal
        KeyCode::Char('-') if app.right_panel_tab == RightPanelTab::Chat => {
            let cid = app.active_coordinator_id;
            let options = vec![
                ('a', "Archive".into(), "Mark as done — work complete".into()),
                (
                    's',
                    "Stop".into(),
                    "Pause coordinator — resume later".into(),
                ),
                ('x', "Abandon".into(), "Permanently discard".into()),
            ];
            app.input_mode = InputMode::ChoiceDialog(ChoiceDialogState {
                action: ChoiceDialogAction::RemoveCoordinator(cid),
                selected: 0,
                options,
            });
        }

        // Task tabs: '[' browses to older iteration
        KeyCode::Char('[')
            if matches!(
                app.right_panel_tab,
                RightPanelTab::Detail | RightPanelTab::Log | RightPanelTab::Messages
            ) =>
        {
            if app.iteration_prev() {
                handle_iteration_change(app);
                let total = app.iteration_archives.len() + 1;
                let msg = match app.viewing_iteration {
                    Some(idx) => format!("Viewing iteration {}/{}", idx + 1, total),
                    None => format!("Viewing current ({}/{})", total, total),
                };
                app.push_toast(msg, super::state::ToastSeverity::Info);
            }
        }
        // Task tabs: ']' browses to newer iteration
        KeyCode::Char(']')
            if matches!(
                app.right_panel_tab,
                RightPanelTab::Detail | RightPanelTab::Log | RightPanelTab::Messages
            ) =>
        {
            if app.iteration_next() {
                handle_iteration_change(app);
                let total = app.iteration_archives.len() + 1;
                let msg = match app.viewing_iteration {
                    Some(idx) => format!("Viewing iteration {}/{}", idx + 1, total),
                    None => format!("Viewing current ({}/{})", total, total),
                };
                app.push_toast(msg, super::state::ToastSeverity::Info);
            }
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

        // Chat tab: '/' opens in-chat search, Ctrl+F also works
        KeyCode::Char('/') if app.right_panel_tab == RightPanelTab::Chat => {
            app.chat.search.query.clear();
            app.chat.search.matches.clear();
            app.chat.search.current_match = None;
            app.input_mode = InputMode::ChatSearch;
        }
        KeyCode::Char('f')
            if modifiers.contains(KeyModifiers::CONTROL)
                && app.right_panel_tab == RightPanelTab::Chat =>
        {
            app.chat.search.query.clear();
            app.chat.search.matches.clear();
            app.chat.search.current_match = None;
            app.input_mode = InputMode::ChatSearch;
        }

        // Chat tab: 'n' / 'N' navigate between search matches (after search accepted)
        KeyCode::Char('n')
            if app.right_panel_tab == RightPanelTab::Chat
                && !app.chat.search.matches.is_empty() =>
        {
            app.chat_search_next();
        }
        KeyCode::Char('N')
            if app.right_panel_tab == RightPanelTab::Chat
                && !app.chat.search.matches.is_empty() =>
        {
            app.chat_search_prev();
        }

        _ => {}
    }
}

/// Check if the chat is scrolled near the top of loaded messages and load more history if needed.
/// Called after any scroll-up action in the chat panel.
fn maybe_load_more_chat_history(app: &mut VizApp) {
    if !app.chat.has_more_history {
        return;
    }
    // Trigger load when we're within one viewport of the top of loaded messages.
    let total = app.chat.total_rendered_lines;
    let viewport = app.chat.viewport_height.max(1);
    let max_scroll_from_bottom = total.saturating_sub(viewport);
    let clamped_scroll = app.chat.scroll.min(max_scroll_from_bottom);
    let scroll_from_top = max_scroll_from_bottom.saturating_sub(clamped_scroll);
    // Load more when within one viewport height of the top.
    if scroll_from_top < viewport {
        let old_msg_count = app.chat.messages.len();
        if app.load_more_chat_history() {
            // Adjust scroll to maintain the user's visual position after prepending messages.
            // The new messages added at the top will add rendered lines, so we need to
            // increase the scroll-from-bottom by the approximate number of new lines.
            // Since we don't know the exact rendered line count yet (that happens during
            // rendering), we estimate based on message count change.
            let new_msg_count = app.chat.messages.len();
            let added_msgs = new_msg_count.saturating_sub(old_msg_count);
            // Rough estimate: ~3 rendered lines per message (header + content + blank).
            let estimated_new_lines = added_msgs * 3;
            app.chat.scroll = app.chat.scroll.saturating_add(estimated_new_lines);
        }
    }
}

/// Phase 3c: poll for the external handler to release the session
/// lock after the user sent a message in observer mode.
///
/// Returns `true` if state changed (so the caller triggers a
/// redraw). Returns `false` if nothing is pending OR the handler
/// hasn't released yet.
///
/// Timeout: 15s. If the handler is mid-tool-call it may not release
/// sooner; the design (sessions-as-identity.md §Long tool calls in
/// progress) explicitly prefers journal consistency over UI
/// snappiness. On timeout we drop the pending state but keep the
/// observer pane — the user can re-send or wait.
fn poll_chat_pty_takeover(app: &mut VizApp) -> bool {
    let since = match app.chat_pty_takeover_pending_since {
        Some(t) => t,
        None => return false,
    };
    let task_id = format!(".coordinator-{}", app.active_coordinator_id);
    let chat_dir = app.workgraph_dir.join("chat").join(&task_id);
    // Has the handler released?
    let released = match workgraph::session_lock::read_holder(&chat_dir) {
        Ok(None) => true,
        Ok(Some(info)) => !info.alive,
        Err(_) => false,
    };
    let timed_out = since.elapsed() > std::time::Duration::from_secs(15);

    if !released && !timed_out {
        return false;
    }

    app.chat_pty_takeover_pending_since = None;

    if timed_out && !released {
        eprintln!(
            "[tui] takeover timed out for {} — handler still busy; \
             retry by sending another message.",
            task_id
        );
        return true;
    }

    // Lock is free. Drop observer pane and spawn owner.
    app.task_panes.remove(&task_id);
    app.chat_pty_observer = false;
    // Clear any stale release marker so our new handler doesn't
    // immediately exit upon seeing it.
    workgraph::session_lock::clear_release_marker(&chat_dir);

    let self_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "wg".to_string());
    let env: Vec<(String, String)> = vec![(
        "WG_DIR".to_string(),
        app.workgraph_dir.display().to_string(),
    )];
    match crate::tui::pty_pane::PtyPane::spawn(
        &self_exe,
        &["spawn-task", &task_id],
        &env,
        24,
        80,
    ) {
        Ok(pane) => {
            app.task_panes.insert(task_id, pane);
        }
        Err(e) => {
            eprintln!("[tui] takeover spawn failed: {}", e);
            app.chat_pty_mode = false;
        }
    }
    true
}

/// Toggle PTY-backed rendering for the active coordinator's chat.
///
/// On first toggle-on, spawn `wg spawn-task .coordinator-<id>` as a
/// PTY child and cache the pane in `app.task_panes`. On toggle-off,
/// the pane stays in the map so re-enabling is instant (no respawn,
/// preserving scrollback and partial input).
///
/// Phase 3a: owner mode only — we assume nothing else currently owns
/// the handler. Phase 3b will add the lock-held observer path;
/// Phase 3c will wire takeover-on-send.
fn toggle_chat_pty_mode(app: &mut VizApp) {
    app.chat_pty_mode = !app.chat_pty_mode;
    if !app.chat_pty_mode {
        return;
    }
    let task_id = format!(".coordinator-{}", app.active_coordinator_id);
    // Already have a live pane? Keep it.
    if let Some(p) = app.task_panes.get_mut(&task_id) {
        if p.is_alive() {
            return;
        }
        // Dead — drop so we can respawn fresh.
        app.task_panes.remove(&task_id);
    }

    // Decide spawn mode by lock state. Observer mode (another
    // handler already owns the session) spawns `wg session attach`
    // which tails the streaming/outbox files read-only. Owner mode
    // (no current handler) spawns `wg spawn-task` which acquires
    // the lock and runs the real handler. Phase 3c will wire the
    // takeover-on-send path that bridges from observer → owner.
    let chat_dir = app.workgraph_dir.join("chat").join(&task_id);
    let observer_mode = workgraph::session_lock::read_holder(&chat_dir)
        .ok()
        .flatten()
        .is_some_and(|info| info.alive);
    app.chat_pty_observer = observer_mode;

    let self_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "wg".to_string());
    let env: Vec<(String, String)> = vec![(
        "WG_DIR".to_string(),
        app.workgraph_dir.display().to_string(),
    )];

    let args: Vec<&str> = if observer_mode {
        vec!["session", "attach", &task_id]
    } else {
        vec!["spawn-task", &task_id]
    };

    match crate::tui::pty_pane::PtyPane::spawn(&self_exe, &args, &env, 24, 80) {
        Ok(pane) => {
            app.task_panes.insert(task_id, pane);
        }
        Err(e) => {
            eprintln!(
                "[tui] failed to spawn {} pane for {}: {}",
                if observer_mode { "observer" } else { "owner" },
                task_id,
                e
            );
            app.chat_pty_mode = false;
        }
    }
}

fn right_panel_scroll_up(app: &mut VizApp, amount: usize) {
    app.record_panel_scroll_activity();
    match app.right_panel_tab {
        RightPanelTab::Detail => app.hud_scroll_up(amount),
        RightPanelTab::Chat => {
            app.chat.scroll += amount;
            // Lazy-load older messages when scrolling near the top of loaded history.
            maybe_load_more_chat_history(app);
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
            if !app.activity_feed.events.is_empty() {
                app.activity_feed_scroll_up(amount);
            } else {
                app.coord_log_scroll_up(amount);
            }
        }
        RightPanelTab::Firehose => {
            app.firehose.auto_tail = false;
            app.firehose.scroll = app.firehose.scroll.saturating_sub(amount);
        }
        RightPanelTab::Output => {
            if let Some(ref agent_id) = app.output_pane.active_agent_id.clone() {
                let scroll_state = app
                    .output_pane
                    .agent_scrolls
                    .entry(agent_id.clone())
                    .or_default();
                scroll_state.scroll = scroll_state.scroll.saturating_sub(amount);
                if scroll_state.scroll == 0 {
                    // At top — auto_follow definitely off
                }
                scroll_state.auto_follow = false;
            }
        }
        RightPanelTab::Dashboard => {
            app.dashboard.selected_row = app.dashboard.selected_row.saturating_sub(amount);
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
            if !app.activity_feed.events.is_empty() {
                app.activity_feed_scroll_down(amount);
            } else {
                app.coord_log_scroll_down(amount);
            }
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
        RightPanelTab::Output => {
            if let Some(ref agent_id) = app.output_pane.active_agent_id.clone() {
                let scroll_state = app
                    .output_pane
                    .agent_scrolls
                    .entry(agent_id.clone())
                    .or_default();
                scroll_state.scroll += amount;
                let max = app
                    .output_pane
                    .total_rendered_lines
                    .saturating_sub(app.output_pane.viewport_height);
                if scroll_state.scroll >= max {
                    scroll_state.scroll = max;
                    scroll_state.auto_follow = true;
                    app.output_pane.has_new_content = false;
                }
            }
        }
        RightPanelTab::Dashboard => {
            let max = app.dashboard.agent_rows.len().saturating_sub(1);
            app.dashboard.selected_row = (app.dashboard.selected_row + amount).min(max);
        }
    }
}

fn right_panel_scroll_to_top(app: &mut VizApp) {
    app.record_panel_scroll_activity();
    match app.right_panel_tab {
        RightPanelTab::Detail => {
            app.hud_scroll = 0;
            app.hud_follow = false;
        }
        RightPanelTab::Chat => {
            // Load all remaining history when jumping to top.
            while app.chat.has_more_history {
                if !app.load_more_chat_history() {
                    break;
                }
            }
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
            if !app.activity_feed.events.is_empty() {
                app.activity_feed_scroll_to_top();
            } else {
                app.coord_log_scroll_to_top();
            }
        }
        RightPanelTab::Firehose => {
            app.firehose.auto_tail = false;
            app.firehose.scroll = 0;
        }
        RightPanelTab::Output => {
            if let Some(ref agent_id) = app.output_pane.active_agent_id.clone() {
                let scroll_state = app
                    .output_pane
                    .agent_scrolls
                    .entry(agent_id.clone())
                    .or_default();
                scroll_state.scroll = 0;
                scroll_state.auto_follow = false;
            }
        }
        RightPanelTab::Dashboard => {
            app.dashboard.selected_row = 0;
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
            if !app.activity_feed.events.is_empty() {
                app.activity_feed_scroll_to_bottom();
            } else {
                app.coord_log_scroll_to_bottom();
            }
        }
        RightPanelTab::Firehose => {
            app.firehose.auto_tail = true;
            app.firehose.scroll = usize::MAX;
        }
        RightPanelTab::Output => {
            if let Some(ref agent_id) = app.output_pane.active_agent_id.clone() {
                let scroll_state = app
                    .output_pane
                    .agent_scrolls
                    .entry(agent_id.clone())
                    .or_default();
                scroll_state.scroll = usize::MAX;
                scroll_state.auto_follow = true;
                app.output_pane.has_new_content = false;
            }
        }
        RightPanelTab::Dashboard => {
            app.dashboard.selected_row = app.dashboard.agent_rows.len().saturating_sub(1);
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
    let in_coordinator_bar =
        app.last_coordinator_bar_area.height > 0 && app.last_coordinator_bar_area.contains(pos);
    let in_divider = app.last_divider_area.width > 0 && app.last_divider_area.contains(pos);
    let in_horizontal_divider = app.last_horizontal_divider_area.height > 0
        && app.last_horizontal_divider_area.contains(pos);
    let in_minimized_strip =
        app.last_minimized_strip_area.width > 0 && app.last_minimized_strip_area.contains(pos);
    let in_fullscreen_restore = app.last_fullscreen_restore_area.width > 0
        && app.last_fullscreen_restore_area.contains(pos);
    let in_fullscreen_right = app.last_fullscreen_right_border_area.width > 0
        && app.last_fullscreen_right_border_area.contains(pos);
    let in_fullscreen_top = app.last_fullscreen_top_border_area.height > 0
        && app.last_fullscreen_top_border_area.contains(pos);
    let in_fullscreen_bottom = app.last_fullscreen_bottom_border_area.height > 0
        && app.last_fullscreen_bottom_border_area.contains(pos);

    // Track hover state for the dividers (visual indicator).
    app.divider_hover = in_divider || app.scrollbar_drag == Some(ScrollbarDragTarget::Divider);
    app.horizontal_divider_hover =
        in_horizontal_divider || app.scrollbar_drag == Some(ScrollbarDragTarget::HorizontalDivider);
    // Track hover state for tri-state strips.
    app.minimized_strip_hover = in_minimized_strip;
    app.fullscreen_restore_hover = in_fullscreen_restore;
    app.fullscreen_right_hover = in_fullscreen_right;
    app.fullscreen_top_hover = in_fullscreen_top;
    app.fullscreen_bottom_hover = in_fullscreen_bottom;

    match kind {
        MouseEventKind::ScrollUp => {
            if in_text_prompt {
                // Scroll up in text prompt: move cursor up to trigger viewport change.
                scroll_editor_up(app, 3, EditorTarget::TextPrompt);
            } else if in_graph && app.scroll_axis_swapped {
                // Axis-swap mode: vertical scroll → horizontal scroll in graph.
                app.record_graph_hscroll_activity();
                app.scroll.scroll_left(3);
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
            } else if in_graph && app.scroll_axis_swapped {
                // Axis-swap mode: vertical scroll → horizontal scroll in graph.
                app.record_graph_hscroll_activity();
                app.scroll.scroll_right(3);
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
            // Touch echo: record click position for visual feedback overlay.
            app.add_touch_echo(column, row);

            // Service health badge click
            let in_service_badge =
                app.last_service_badge_area.width > 0 && app.last_service_badge_area.contains(pos);
            if in_service_badge {
                app.toggle_service_control_panel();
                return;
            }
            if in_coordinator_bar {
                // Click on coordinator/user-board tab bar.
                app.focused_panel = FocusedPanel::RightPanel;

                // Check [+] button first
                let plus = &app.coordinator_plus_hit;
                if column >= plus.start && column < plus.end {
                    app.right_panel_tab = RightPanelTab::Chat;
                    super::state::editor_clear(&mut app.text_prompt.editor);
                    app.input_mode = InputMode::TextPrompt(TextPromptAction::CreateCoordinator);
                    return;
                }

                // Check each tab's hit area
                // Clone tab_hits to avoid borrow conflict with app methods.
                let tab_hits: Vec<_> = app.coordinator_tab_hits.clone();
                for hit in &tab_hits {
                    if column >= hit.tab_start && column < hit.tab_end {
                        match &hit.kind {
                            TabBarEntryKind::Coordinator(cid) => {
                                app.right_panel_tab = RightPanelTab::Chat;
                                // Check if click is on the close button — open choice dialog
                                if hit.close_start != hit.close_end
                                    && column >= hit.close_start
                                    && column < hit.close_end
                                {
                                    let cid = *cid;
                                    let options = vec![
                                        (
                                            'a',
                                            "Archive".into(),
                                            "Mark as done — work complete".into(),
                                        ),
                                        (
                                            's',
                                            "Stop".into(),
                                            "Pause coordinator — resume later".into(),
                                        ),
                                        ('x', "Abandon".into(), "Permanently discard".into()),
                                    ];
                                    app.input_mode = InputMode::ChoiceDialog(ChoiceDialogState {
                                        action: ChoiceDialogAction::RemoveCoordinator(cid),
                                        selected: 0,
                                        options,
                                    });
                                } else {
                                    app.switch_coordinator(*cid);
                                }
                            }
                            TabBarEntryKind::UserBoard(task_id) => {
                                // Select the user board task and switch to Messages tab.
                                let task_id = task_id.clone();
                                if let Some(idx) =
                                    app.task_order.iter().position(|id| *id == task_id)
                                {
                                    app.selected_task_idx = Some(idx);
                                    app.recompute_trace();
                                    app.scroll_to_selected_task();
                                }
                                app.right_panel_tab = RightPanelTab::Messages;
                            }
                        }
                        return;
                    }
                }
                return;
            }
            if in_minimized_strip {
                // Click on minimized strip: restore to last normal split mode.
                app.restore_from_extreme();
            } else if in_fullscreen_restore {
                // Click on full-screen restore strip: transition to normal split
                // and start divider drag so user can fine-tune position.
                // Place the divider at the current visual border (right edge of
                // the restore strip) instead of the click column, so the panel
                // width is preserved and there is no resize jump on click.
                // The drag offset captures where the user grabbed relative to
                // the border so subsequent drag events feel anchored.
                app.right_panel_visible = true;
                let total_width = {
                    let restore_w = app.last_fullscreen_restore_area.width;
                    let right_w = app.last_fullscreen_right_border_area.width;
                    app.last_right_panel_area.width + restore_w + right_w
                }
                .max(1);
                let left_x = app.last_fullscreen_restore_area.x;
                let right_edge = left_x + total_width;
                // Use the visual border position (not the click column) so the
                // panel doesn't shrink on initial mousedown.
                let border_col =
                    app.last_fullscreen_restore_area.x + app.last_fullscreen_restore_area.width;
                let panel_width = right_edge.saturating_sub(border_col);
                let pct = ((panel_width as u32 * 100) / total_width as u32).clamp(1, 99) as u16;
                app.right_panel_percent = pct;
                app.layout_mode = super::state::VizApp::layout_mode_for_percent(pct);
                if pct > 0 && pct < 100 {
                    app.last_split_percent = pct;
                    app.last_split_mode = app.layout_mode;
                }
                // Pre-update layout areas so the drag handler can compute
                // consistent total_width before the next render frame
                // (graph_area is still empty from FullInspector mode).
                let right_width = (total_width as u32 * pct as u32 / 100) as u16;
                let left_width = total_width.saturating_sub(right_width);
                app.last_graph_area.x = left_x;
                app.last_graph_area.width = left_width;
                let new_panel_x = left_x + left_width;
                app.last_right_panel_area.x = new_panel_x;
                app.last_right_panel_area.width = right_width;
                // Offset: click position relative to the new divider column,
                // so subsequent drags track relative to the grab point.
                app.divider_drag_offset = column as i16 - new_panel_x as i16;
                app.divider_drag_start_pct = pct;
                app.divider_drag_start_col = column;
                app.scrollbar_drag = Some(ScrollbarDragTarget::Divider);
            } else if in_fullscreen_top {
                // Click on full-screen top border: transition to stacked split
                // and start horizontal divider drag so user can fine-tune position.
                app.right_panel_visible = true;
                let total_height = {
                    let top_h = app.last_fullscreen_top_border_area.height;
                    let bottom_h = app.last_fullscreen_bottom_border_area.height;
                    app.last_right_panel_area.height + top_h + bottom_h
                }
                .max(1);
                let border_row = app.last_fullscreen_top_border_area.y
                    + app.last_fullscreen_top_border_area.height;
                let panel_height = (app.last_fullscreen_top_border_area.y + total_height)
                    .saturating_sub(border_row);
                let pct = ((panel_height as u32 * 100) / total_height as u32).clamp(1, 99) as u16;
                app.right_panel_percent = pct;
                app.layout_mode = super::state::VizApp::layout_mode_for_percent(pct);
                if pct > 0 && pct < 100 {
                    app.last_split_percent = pct;
                    app.last_split_mode = app.layout_mode;
                }
                // Pre-update layout areas so drag handler has consistent total_height.
                let panel_h = (total_height as u32 * pct as u32 / 100) as u16;
                let graph_h = total_height.saturating_sub(panel_h);
                app.last_graph_area.y = app.last_fullscreen_top_border_area.y;
                app.last_graph_area.height = graph_h;
                app.last_right_panel_area.y = app.last_fullscreen_top_border_area.y + graph_h;
                app.last_right_panel_area.height = panel_h;
                app.inspector_is_beside = false;
                app.divider_drag_start_pct = pct;
                app.divider_drag_start_row = row;
                app.scrollbar_drag = Some(ScrollbarDragTarget::HorizontalDivider);
            } else if in_fullscreen_right || in_fullscreen_bottom {
                // Click on other fullscreen borders: restore to normal split.
                app.restore_from_extreme();
            } else if in_graph_vscrollbar {
                // Click on graph vertical scrollbar: start drag and jump.
                // Checked before in_divider because the scrollbar column overlaps
                // the wide (3-col) divider grab zone.
                app.focused_panel = FocusedPanel::Graph;
                app.scrollbar_drag = Some(ScrollbarDragTarget::Graph);
                app.record_graph_scroll_activity();
                vscrollbar_jump_graph(app, row);
            } else if in_panel_vscrollbar {
                // Click on panel vertical scrollbar: start drag and jump.
                // Checked before in_divider for the same overlap reason.
                app.focused_panel = FocusedPanel::RightPanel;
                app.scrollbar_drag = Some(ScrollbarDragTarget::Panel);
                app.record_panel_scroll_activity();
                vscrollbar_jump_panel(app, row);
            } else if in_tab_bar {
                // Click on tab header: always focus right panel, switch tab if hit.
                // Checked before divider handlers because the horizontal divider's
                // 3-row grab zone can overlap the tab bar row in stacked mode.
                app.focused_panel = FocusedPanel::RightPanel;

                // Check for iteration navigator click first
                let in_iteration_nav = app.last_iteration_nav_area.width > 0
                    && app.last_iteration_nav_area.contains(pos);

                if in_iteration_nav {
                    handle_iteration_navigator_click(app, column);
                } else {
                    let col_in_tabs = column.saturating_sub(app.last_tab_bar_area.x);
                    if let Some(tab) = tab_at_column(col_in_tabs) {
                        // Special behavior for Log tab: toggle view mode if already active
                        if tab == RightPanelTab::Log && app.right_panel_tab == RightPanelTab::Log {
                            app.toggle_log_view();
                        } else {
                            app.right_panel_tab = tab;
                        }
                    }
                }
            } else if in_divider {
                // Click on divider between graph and inspector: start resize drag.
                // Record the starting percent and column so the drag handler can
                // use delta-based calculation, avoiding the lossy percent↔width
                // round-trip that causes an initial snap on drag start.
                app.divider_drag_start_pct = app.right_panel_percent;
                app.divider_drag_start_col = column;
                app.scrollbar_drag = Some(ScrollbarDragTarget::Divider);
            } else if in_horizontal_divider {
                // Click on horizontal divider (stacked mode): start resize drag.
                app.divider_drag_start_pct = app.right_panel_percent;
                app.divider_drag_start_row = row;
                app.scrollbar_drag = Some(ScrollbarDragTarget::HorizontalDivider);
            } else if in_graph_hscrollbar {
                app.focused_panel = FocusedPanel::Graph;
                app.scrollbar_drag = Some(ScrollbarDragTarget::GraphHorizontal);
                app.record_graph_hscroll_activity();
                hscrollbar_jump_to_column(app, column);
            } else if in_text_prompt {
                // Click inside text prompt overlay: position cursor via edtui.
                route_mouse_to_editor(app, row, column, EditorTarget::TextPrompt);
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
                // Click on chat message history area.
                app.focused_panel = FocusedPanel::RightPanel;
                // Determine which rendered line was clicked.
                let click_row = (row.saturating_sub(app.last_chat_message_area.y)) as usize;
                let rendered_line_idx = app.chat.scroll_from_top + click_row;
                // Check if the clicked line maps to an editable user message.
                let clicked_msg_idx = app
                    .chat
                    .line_to_message
                    .get(rendered_line_idx)
                    .copied()
                    .flatten();
                if let Some(msg_idx) = clicked_msg_idx
                    && !app.is_chat_message_consumed(msg_idx)
                    && app
                        .chat
                        .messages
                        .get(msg_idx)
                        .is_some_and(|m| m.role == super::state::ChatRole::User)
                {
                    // Click on an editable user message: enter edit mode.
                    app.enter_chat_edit_mode(msg_idx);
                    app.input_mode = InputMode::ChatInput;
                    app.chat_input_dismissed = false;
                    app.inspector_sub_focus = InspectorSubFocus::TextEntry;
                    return;
                }
                // Default: focus history, exit text editing.
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
            } else if app.last_log_new_output_area.width > 0
                && app.last_log_new_output_area.contains(pos)
            {
                // Click on "▼ new output" indicator in Log tab: scroll to bottom.
                app.focused_panel = FocusedPanel::RightPanel;
                app.log_scroll_to_bottom();
            } else if app.last_iter_nav_area.width > 0
                && app.last_iter_nav_area.contains(pos)
                && app.right_panel_tab == RightPanelTab::Detail
            {
                // Click on ◀ ▶ iteration navigation in Detail tab header.
                app.focused_panel = FocusedPanel::RightPanel;
                let col = column.saturating_sub(app.last_iter_nav_area.x);
                let total = app.iteration_archives.len() + 1;
                let usable_width = app.last_iter_nav_area.width.saturating_sub(2) as usize;
                let center_len = format!(" iter {}/{} ", total, total).len();
                let arrow_width = 2; // ◀ or ▶
                let gap = 2;
                let side_width =
                    (usable_width.saturating_sub(center_len + arrow_width * 2 + gap * 2)) / 2;

                // ◀ is at position side_width (with leading space)
                // ▶ is at position side_width + center_len + gap * 2 + arrow_width
                let left_arrow_end = 1 + side_width;
                let right_arrow_start = 1 + side_width + center_len + gap * 2;

                if usize::from(col) <= left_arrow_end {
                    // Click on ◀: go to previous iteration
                    if app.iteration_prev() {
                        app.load_hud_detail();
                        let msg = match app.viewing_iteration {
                            Some(idx) => format!("Viewing iteration {}/{}", idx + 1, total),
                            None => format!("Viewing current ({}/{})", total, total),
                        };
                        app.push_toast(msg, super::state::ToastSeverity::Info);
                    }
                } else if usize::from(col) >= right_arrow_start {
                    // Click on ▶: go to next iteration
                    if app.iteration_next() {
                        app.load_hud_detail();
                        let msg = match app.viewing_iteration {
                            Some(idx) => format!("Viewing iteration {}/{}", idx + 1, total),
                            None => format!("Viewing current ({}/{})", total, total),
                        };
                        app.push_toast(msg, super::state::ToastSeverity::Info);
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
                        let content_col = (column.saturating_sub(app.last_graph_area.x) as usize)
                            + app.scroll.offset_x;

                        // Check annotation hit regions first (pink phase labels).
                        let clicked_annotation = app
                            .annotation_hit_regions
                            .iter()
                            .find(|r| {
                                r.orig_line == orig_line
                                    && content_col >= r.col_start
                                    && content_col < r.col_end
                            })
                            .cloned();

                        if let Some(region) = clicked_annotation {
                            // Select the parent task (keeps graph node highlighted).
                            app.select_task_at_line(orig_line);
                            // Show the dot-task detail in the inspector.
                            if let Some(dot_id) = region.dot_task_ids.first() {
                                app.load_hud_detail_for_task(dot_id);
                            }
                            app.right_panel_visible = true;
                            app.right_panel_tab = RightPanelTab::Detail;
                            // Agency annotations → fullscreen so logs/scores are
                            // immediately readable without manual resizing.
                            if region.dot_task_ids.iter().any(|id| is_agency_task_id(id)) {
                                app.apply_layout_mode(super::state::LayoutMode::FullInspector);
                            }
                            // Trigger annotation flash.
                            app.annotation_click_flash = Some(super::state::AnnotationClickFlash {
                                orig_line: region.orig_line,
                                col_start: region.col_start,
                                col_end: region.col_end,
                                start: std::time::Instant::now(),
                            });
                        } else {
                            // Only select a task when clicking on actual text content
                            // (task name, status, log snippet, mail indicator), not on
                            // tree-drawing chars, indentation, or empty space past the
                            // end of the line.  This prevents accidental selection
                            // changes when click-dragging to pan.
                            let on_text = app
                            .plain_lines
                            .get(orig_line)
                            .map(|line| {
                                let chars: Vec<char> = line.chars().collect();
                                let text_start =
                                    chars.iter().position(|c| c.is_alphanumeric() || is_message_indicator(*c));
                                let text_end = chars
                                    .iter()
                                    .rposition(|c| !c.is_whitespace())
                                    .map(|p| p + 1)
                                    .unwrap_or(0);
                                matches!(text_start, Some(ts) if content_col >= ts && content_col < text_end)
                            })
                            .unwrap_or(false);

                            if on_text {
                                // Check if the click is on the mail indicator (✉) region.
                                let clicked_mail = app
                                    .plain_lines
                                    .get(orig_line)
                                    .and_then(|line| {
                                        let envelope_char_col = line
                                            .char_indices()
                                            .position(|(_, c)| is_message_indicator(c))?;
                                        let after_envelope: String = line
                                            .chars()
                                            .skip(envelope_char_col + 1)
                                            .take_while(|c| !c.is_whitespace())
                                            .collect();
                                        let end_col =
                                            envelope_char_col + 1 + after_envelope.chars().count();
                                        if content_col >= envelope_char_col && content_col < end_col
                                        {
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
                        } // end else (not annotation click)
                    }
                }
            } else if app.last_right_panel_area.contains(pos) {
                // Click on right panel border area: focus right panel.
                app.focused_panel = FocusedPanel::RightPanel;
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if app.scrollbar_drag == Some(ScrollbarDragTarget::Divider) {
                // Dragging the divider: compute new right_panel_percent from mouse column.
                // The right panel starts at `column` and extends to the right edge.
                // Use graph+panel width when available, fall back to total known area.
                let total_width =
                    if app.last_graph_area.width > 0 && app.last_right_panel_area.width > 0 {
                        app.last_graph_area.width + app.last_right_panel_area.width
                    } else if app.last_graph_area.width > 0 {
                        // Coming from FullInspector restore — panel area not yet set.
                        // Estimate total from graph area + rough frame width.
                        app.last_graph_area.width.max(80)
                    } else if app.last_right_panel_area.width > 0 {
                        app.last_right_panel_area.width.max(80)
                    } else {
                        0
                    };
                if total_width > 0 {
                    // Delta-based percent calculation: compute how far the mouse
                    // moved from the drag start column and convert that to a
                    // percent change.  This avoids the lossy percent↔width
                    // round-trip (integer division) that caused an initial snap
                    // when the divider drag started.
                    let delta = column as i32 - app.divider_drag_start_col as i32;
                    let delta_pct = delta * 100 / total_width as i32;
                    let pct = (app.divider_drag_start_pct as i32 - delta_pct)
                        .clamp(MIN_DRAG_PERCENT, 100) as u16;
                    app.right_panel_percent = pct;
                    app.right_panel_visible = true;
                    // Preserve last non-extreme split state for restore.
                    if pct > 0 && pct < 100 {
                        app.last_split_percent = pct;
                        app.last_split_mode = app.layout_mode;
                    }
                    // Map to a normal-split LayoutMode (avoid FullInspector/Off
                    // during drag — those modes restructure layout areas).
                    app.layout_mode = if pct >= 100 {
                        super::state::LayoutMode::TwoThirdsInspector
                    } else {
                        super::state::VizApp::layout_mode_for_percent(pct)
                    };
                }
            } else if app.scrollbar_drag == Some(ScrollbarDragTarget::HorizontalDivider) {
                // Dragging the horizontal divider (stacked mode): compute new
                // right_panel_percent from mouse row.
                let total_height =
                    if app.last_graph_area.height > 0 && app.last_right_panel_area.height > 0 {
                        app.last_graph_area.height + app.last_right_panel_area.height
                    } else {
                        0
                    };
                if total_height > 0 {
                    // Delta-based percent: dragging DOWN (positive delta) shrinks
                    // the inspector (bottom panel), dragging UP grows it.
                    let delta = row as i32 - app.divider_drag_start_row as i32;
                    let delta_pct = delta * 100 / total_height as i32;
                    let pct = (app.divider_drag_start_pct as i32 - delta_pct)
                        .clamp(MIN_DRAG_PERCENT, 100) as u16;
                    app.right_panel_percent = pct;
                    app.right_panel_visible = true;
                    if pct > 0 && pct < 100 {
                        app.last_split_percent = pct;
                        app.last_split_mode = app.layout_mode;
                    }
                    app.layout_mode = if pct >= 100 {
                        super::state::LayoutMode::TwoThirdsInspector
                    } else {
                        super::state::VizApp::layout_mode_for_percent(pct)
                    };
                }
            } else if app.scrollbar_drag == Some(ScrollbarDragTarget::Graph) {
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
            // Finalize layout mode when divider drag ends at an extreme.
            if app.scrollbar_drag == Some(ScrollbarDragTarget::Divider)
                || app.scrollbar_drag == Some(ScrollbarDragTarget::HorizontalDivider)
            {
                if app.right_panel_percent >= 100 {
                    app.layout_mode = super::state::LayoutMode::FullInspector;
                    app.right_panel_visible = true;
                    app.focused_panel = super::state::FocusedPanel::RightPanel;
                } else if app.right_panel_percent == 0 {
                    app.layout_mode = super::state::LayoutMode::Off;
                    app.right_panel_visible = false;
                    app.focused_panel = super::state::FocusedPanel::Graph;
                }
            }
            if app.scrollbar_drag.is_some() {
                app.scrollbar_drag = None;
                app.divider_drag_offset = 0;
                app.divider_drag_start_pct = 0;
                app.divider_drag_start_col = 0;
                app.divider_drag_start_row = 0;
            }
            app.graph_pan_last = None;
        }
        // Moved events (mode 1003): treat as drag-to-pan when a touch/click is
        // active.  Termux touch-to-mouse translation may report motion without
        // the button-held flag, producing Moved instead of Drag(Left).  With
        // mode 1003 enabled (auto for Termux), these events keep panning alive.
        MouseEventKind::Moved if app.graph_pan_last.is_some() => {
            if let Some((prev_col, prev_row)) = app.graph_pan_last {
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
            maybe_load_more_chat_history(app);
        }
        RightPanelTab::Log => {
            let total = app.log_pane.total_wrapped_lines;
            let viewport = app.log_pane.viewport_height;
            let max_scroll = total.saturating_sub(viewport);
            if max_scroll == 0 {
                return;
            }
            app.log_pane.scroll = jump(max_scroll);
            // Update auto-tail based on whether the user dragged to the bottom.
            if app.log_pane.scroll >= max_scroll {
                app.log_pane.auto_tail = true;
                app.log_pane.has_new_content = false;
            } else {
                app.log_pane.auto_tail = false;
            }
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
            let new_scroll = jump(max_scroll);
            app.coord_log.scroll = new_scroll;
            app.coord_log.auto_tail = new_scroll >= max_scroll;
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
        RightPanelTab::Output => {
            let total = app.output_pane.total_rendered_lines;
            let viewport = app.output_pane.viewport_height;
            let max_scroll = total.saturating_sub(viewport);
            if max_scroll == 0 {
                return;
            }
            if let Some(ref agent_id) = app.output_pane.active_agent_id.clone() {
                let scroll_state = app
                    .output_pane
                    .agent_scrolls
                    .entry(agent_id.clone())
                    .or_default();
                let new_scroll = jump(max_scroll);
                scroll_state.scroll = new_scroll;
                scroll_state.auto_follow = new_scroll >= max_scroll;
                if scroll_state.auto_follow {
                    app.output_pane.has_new_content = false;
                }
            }
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

    // Special case: "+ Add model" entry
    if app.config_panel.entries[idx].key == "model.add" {
        app.config_panel.adding_model = true;
        app.config_panel.new_model = super::state::NewModelFields::default();
        app.config_panel.new_model_field = 0;
        app.config_panel.editing = false;
        app.input_mode = InputMode::ConfigEdit;
        return;
    }

    // Special case: "Remove endpoint" / "Remove model" — just toggle (which triggers removal)
    if app.config_panel.entries[idx].key.ends_with(".remove") {
        app.toggle_config_entry();
        return;
    }

    // Special case: "Set as default" for models — just toggle
    if app.config_panel.entries[idx].key.ends_with(".set_default") {
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

    // Add-model form mode
    if app.config_panel.adding_model {
        handle_add_model_input(app, code, modifiers);
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

/// Handle key events for the add-model form.
fn handle_add_model_input(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
    let field = app.config_panel.new_model_field;

    match code {
        KeyCode::Esc => {
            app.config_panel.adding_model = false;
            app.config_panel.editing = false;
            app.input_mode = InputMode::Normal;
        }
        // Ctrl+S saves the model
        KeyCode::Char('s') if modifiers.contains(KeyModifiers::CONTROL) => {
            if app.config_panel.editing {
                set_model_field(
                    &mut app.config_panel.new_model,
                    field,
                    &app.config_panel.edit_buffer.clone(),
                );
                app.config_panel.editing = false;
            }
            app.add_model();
            app.input_mode = InputMode::Normal;
        }
        // Tab moves to next field
        KeyCode::Tab => {
            if app.config_panel.editing {
                let buf = app.config_panel.edit_buffer.clone();
                set_model_field(&mut app.config_panel.new_model, field, &buf);
                app.config_panel.editing = false;
            }
            app.config_panel.new_model_field = (field + 1) % 5;
        }
        // BackTab moves to previous field
        KeyCode::BackTab => {
            if app.config_panel.editing {
                let buf = app.config_panel.edit_buffer.clone();
                set_model_field(&mut app.config_panel.new_model, field, &buf);
                app.config_panel.editing = false;
            }
            app.config_panel.new_model_field = if field == 0 { 4 } else { field - 1 };
        }
        KeyCode::Enter => {
            if app.config_panel.editing {
                let buf = app.config_panel.edit_buffer.clone();
                set_model_field(&mut app.config_panel.new_model, field, &buf);
                app.config_panel.editing = false;
                if field < 4 {
                    app.config_panel.new_model_field = field + 1;
                } else {
                    app.add_model();
                    app.input_mode = InputMode::Normal;
                }
            } else {
                app.config_panel.edit_buffer = get_model_field(&app.config_panel.new_model, field);
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
            app.config_panel.new_model_field = if field == 0 { 4 } else { field - 1 };
        }
        KeyCode::Down | KeyCode::Char('j') if !app.config_panel.editing => {
            app.config_panel.new_model_field = (field + 1) % 5;
        }
        _ => {}
    }
}

/// Set a field on the new-model form by index.
fn set_model_field(fields: &mut super::state::NewModelFields, idx: usize, val: &str) {
    match idx {
        0 => fields.id = val.to_string(),
        1 => fields.provider = val.to_string(),
        2 => fields.tier = val.to_string(),
        3 => fields.cost_in = val.to_string(),
        4 => fields.cost_out = val.to_string(),
        _ => {}
    }
}

/// Get a field from the new-model form by index.
fn get_model_field(fields: &super::state::NewModelFields, idx: usize) -> String {
    match idx {
        0 => fields.id.clone(),
        1 => fields.provider.clone(),
        2 => fields.tier.clone(),
        3 => fields.cost_in.clone(),
        4 => fields.cost_out.clone(),
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

/// Pop the navigation stack. If non-empty, restore the previous view.
/// If empty, fall back to graph focus (default Esc behavior).
fn nav_stack_pop(app: &mut VizApp) {
    match app.nav_stack.pop() {
        Some(NavEntry::Dashboard) => {
            app.right_panel_tab = RightPanelTab::Dashboard;
        }
        Some(NavEntry::AgentDetail { agent_id }) => {
            app.output_pane.active_agent_id = Some(agent_id);
            app.right_panel_tab = RightPanelTab::Output;
        }
        Some(NavEntry::TaskDetail { task_id }) => {
            app.load_hud_detail_for_task(&task_id);
            app.right_panel_tab = RightPanelTab::Detail;
        }
        Some(NavEntry::TaskLog { task_id }) => {
            if let Some(idx) = app.task_order.iter().position(|id| *id == task_id) {
                app.selected_task_idx = Some(idx);
            }
            app.invalidate_log_pane();
            app.load_log_pane();
            app.right_panel_tab = RightPanelTab::Log;
        }
        None => {
            // No nav history — default to returning to graph focus
            app.focused_panel = FocusedPanel::Graph;
        }
    }
}

/// Check whether a character is a message indicator icon in the viz view.
/// Covers all `CoordinatorMessageStatus` icons: ✉ (Unseen), ↩ (Seen), ✓ (Replied).
fn is_message_indicator(c: char) -> bool {
    matches!(c, '✉' | '↩' | '✓')
}

/// Check whether a task ID belongs to the agency pipeline (internal system tasks
/// whose logs/scores are more useful than their graph position).
fn is_agency_task_id(id: &str) -> bool {
    id.starts_with(".evaluate-")
        || id.starts_with(".assign-")
        || id.starts_with(".place-")
        || id.starts_with(".flip-")
        || id.starts_with(".create-")
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
            VizLayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
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

    #[test]
    fn moved_event_pans_when_graph_pan_last_set() {
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        app.scroll.content_width = 200;
        app.scroll.viewport_width = 80;
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        app.last_graph_scrollbar_area = Rect::default();
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Simulate touch down in graph area — sets graph_pan_last.
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 40);
        assert!(
            app.graph_pan_last.is_some(),
            "Pan anchor should be set on mouse down in graph"
        );

        // Moved event (Termux touch drag without button flag) should pan.
        handle_mouse(&mut app, MouseEventKind::Moved, 5, 30);
        // Dragged up by 5 rows: dy = 10-5 = 5 > 0 → scroll_down(5)
        assert_eq!(app.scroll.offset_y, 5, "Vertical pan via Moved event");
        // Dragged left by 10 cols: dx = 40-30 = 10 > 0 → scroll_right(10)
        assert_eq!(app.scroll.offset_x, 10, "Horizontal pan via Moved event");

        // Mouse up clears pan state.
        handle_mouse(&mut app, MouseEventKind::Up(MouseButton::Left), 5, 30);
        assert!(
            app.graph_pan_last.is_none(),
            "Pan state should be cleared on mouse up"
        );

        // Moved event without prior mouse down should NOT pan.
        let prev_y = app.scroll.offset_y;
        let prev_x = app.scroll.offset_x;
        handle_mouse(&mut app, MouseEventKind::Moved, 0, 0);
        assert_eq!(
            app.scroll.offset_y, prev_y,
            "Moved without graph_pan_last should not scroll vertically"
        );
        assert_eq!(
            app.scroll.offset_x, prev_x,
            "Moved without graph_pan_last should not scroll horizontally"
        );
    }

    // ── 8. Click-select only on text content ──

    /// Helper to set up graph area for click-to-select tests.
    fn setup_for_click_select(app: &mut VizApp) {
        setup_graph_scroll(app, 100, 20);
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
    }

    #[test]
    fn click_on_task_text_selects_task() {
        let (mut app, _tmp) = build_test_app();
        setup_for_click_select(&mut app);
        app.selected_task_idx = None;

        // The first plain_line should be a root task like "task-0 (open) Task 0".
        // Find the column where alphanumeric text starts.
        let first_line = &app.plain_lines[0];
        let text_start = first_line
            .chars()
            .position(|c| c.is_alphanumeric())
            .expect("First line should contain text");

        // Click on the text content area (at text_start column, row 0).
        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            0,
            text_start as u16,
        );
        assert!(
            app.selected_task_idx.is_some(),
            "Clicking on task text should select a task"
        );
    }

    #[test]
    fn click_on_empty_space_past_line_end_does_not_select() {
        let (mut app, _tmp) = build_test_app();
        setup_for_click_select(&mut app);
        app.selected_task_idx = None;

        // Click far to the right of any line content (column 78, well past text).
        let first_line_len = app.plain_lines[0].chars().count();
        let past_end_col = (first_line_len + 5) as u16;
        // Make sure the column is within the graph area.
        if past_end_col < 79 {
            handle_mouse(
                &mut app,
                MouseEventKind::Down(MouseButton::Left),
                0,
                past_end_col,
            );
            assert!(
                app.selected_task_idx.is_none(),
                "Clicking past end of line should not select a task"
            );
        }
    }

    /// Build a graph with parent-child edges (tree chars in output).
    fn build_tree_app() -> (VizApp, tempfile::TempDir) {
        let mut graph = WorkGraph::new();
        let root = make_task_with_status("root", "Root task", Status::Open);
        let mut child1 = make_task_with_status("child1", "Child 1", Status::Open);
        child1.after = vec!["root".to_string()];
        let mut child2 = make_task_with_status("child2", "Child 2", Status::Open);
        child2.after = vec!["root".to_string()];
        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(child1));
        graph.add_node(Node::Task(child2));

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
            VizLayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.workgraph_dir = tmp.path().to_path_buf();
        (app, tmp)
    }

    #[test]
    fn click_on_tree_chars_does_not_select() {
        let (mut app, _tmp) = build_tree_app();
        setup_for_click_select(&mut app);

        // Find a child line that has tree-drawing prefix (e.g. "├→" or "└→").
        let child_line_idx = app
            .plain_lines
            .iter()
            .position(|l| l.contains('├') || l.contains('└'))
            .expect("Should have at least one child line with tree chars");

        let line = &app.plain_lines[child_line_idx];
        let text_start = line.chars().position(|c| c.is_alphanumeric()).unwrap_or(0);

        // Click on the tree-drawing prefix area (column 0, which is before text).
        // First select a different task so we can detect change.
        app.selected_task_idx = Some(0);
        let prev_selected = app.selected_task_idx;

        // Scroll so that child_line_idx is visible at row 0.
        app.scroll.offset_y = child_line_idx;

        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            0, // row
            0, // column — in the tree-drawing area
        );

        // The selection should remain unchanged because we clicked on tree chars.
        assert_eq!(
            app.selected_task_idx, prev_selected,
            "Clicking on tree-drawing chars (col 0, before text_start={}) should not change selection",
            text_start,
        );
    }

    #[test]
    fn click_drag_on_empty_space_does_not_change_selection() {
        let (mut app, _tmp) = build_test_app();
        setup_for_click_select(&mut app);

        // Select task 0 first.
        app.selected_task_idx = Some(0);

        // Click on empty space past line end — should not change selection.
        let first_line_len = app.plain_lines[0].chars().count();
        let past_end_col = (first_line_len + 5) as u16;
        if past_end_col < 79 {
            handle_mouse(
                &mut app,
                MouseEventKind::Down(MouseButton::Left),
                0,
                past_end_col,
            );
            assert_eq!(
                app.selected_task_idx,
                Some(0),
                "Click on empty space should not change selection"
            );

            // Drag should work (pan) without changing selection.
            handle_mouse(
                &mut app,
                MouseEventKind::Drag(MouseButton::Left),
                5,
                past_end_col,
            );
            assert_eq!(
                app.selected_task_idx,
                Some(0),
                "Drag on empty space should not change selection"
            );
        }
    }

    #[test]
    fn scroll_axis_swap_converts_vertical_to_horizontal() {
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
        app.last_graph_scrollbar_area = Rect::default();
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Without axis swap: ScrollDown scrolls vertically.
        handle_mouse(&mut app, MouseEventKind::ScrollDown, 10, 40);
        assert_eq!(
            app.scroll.offset_y, 3,
            "Normal ScrollDown scrolls vertically"
        );
        assert_eq!(
            app.scroll.offset_x, 0,
            "Normal ScrollDown does not scroll horizontally"
        );

        // Reset.
        app.scroll.offset_y = 0;

        // Enable axis swap.
        app.scroll_axis_swapped = true;

        // With axis swap: ScrollDown scrolls horizontally (right).
        handle_mouse(&mut app, MouseEventKind::ScrollDown, 10, 40);
        assert_eq!(
            app.scroll.offset_y, 0,
            "Swapped ScrollDown should not scroll vertically"
        );
        assert_eq!(
            app.scroll.offset_x, 3,
            "Swapped ScrollDown should scroll right"
        );

        // With axis swap: ScrollUp scrolls horizontally (left).
        handle_mouse(&mut app, MouseEventKind::ScrollUp, 10, 40);
        assert_eq!(
            app.scroll.offset_y, 0,
            "Swapped ScrollUp should not scroll vertically"
        );
        assert_eq!(
            app.scroll.offset_x, 0,
            "Swapped ScrollUp should scroll left"
        );
    }

    #[test]
    fn scrollbar_click_wins_over_overlapping_divider() {
        // The graph scrollbar column overlaps with the 3-column-wide divider
        // grab zone. Scrollbar clicks must take priority over the divider.
        let (mut app, _tmp) = build_test_app();
        setup_graph_scroll(&mut app, 100, 20);
        // Graph occupies columns 0–79, scrollbar is the rightmost column (79).
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 20,
        };
        app.last_graph_scrollbar_area = Rect {
            x: 79,
            y: 0,
            width: 1,
            height: 20,
        };
        // Right panel starts at column 80; divider grab zone = columns 79–81.
        // Column 79 overlaps with the scrollbar.
        app.last_divider_area = Rect {
            x: 79,
            y: 0,
            width: 3,
            height: 20,
        };
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();

        // Click on column 79 (the overlapping column).
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 79);
        assert_eq!(
            app.scrollbar_drag,
            Some(ScrollbarDragTarget::Graph),
            "Scrollbar should win over divider when they overlap"
        );
    }

    #[test]
    fn fullscreen_restore_click_does_not_shrink_panel() {
        // Regression: clicking the restore strip in FullInspector mode used to
        // compute panel_width from the click column and clamp pct to 99, causing
        // a ~2 column shrink before the user even moved the mouse.
        let (mut app, _tmp) = build_test_app();

        // Simulate FullInspector layout with a 200-column main area.
        let main_width: u16 = 200;
        let main_height: u16 = 40;
        app.layout_mode = super::super::state::LayoutMode::FullInspector;
        app.right_panel_visible = true;
        // Restore strip: 1 col on the left.
        app.last_fullscreen_restore_area = Rect {
            x: 0,
            y: 0,
            width: 1,
            height: main_height,
        };
        // Right border: 1 col on the right.
        app.last_fullscreen_right_border_area = Rect {
            x: main_width - 1,
            y: 0,
            width: 1,
            height: main_height,
        };
        // Panel content: everything between the borders.
        let panel_content_width = main_width - 2; // 198
        app.last_right_panel_area = Rect {
            x: 1,
            y: 1,
            width: panel_content_width,
            height: main_height - 2,
        };
        app.last_graph_area = Rect::default();
        // Clear any areas that shouldn't be active in FullInspector.
        app.last_divider_area = Rect::default();
        app.last_horizontal_divider_area = Rect::default();
        app.last_graph_scrollbar_area = Rect::default();
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();
        app.last_minimized_strip_area = Rect::default();
        app.last_fullscreen_top_border_area = Rect::default();
        app.last_fullscreen_bottom_border_area = Rect::default();

        // Click on the restore strip (column 0).
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 10, 0);

        // Drag should be initiated.
        assert_eq!(
            app.scrollbar_drag,
            Some(ScrollbarDragTarget::Divider),
            "Clicking restore strip should start divider drag"
        );

        // The panel percent should preserve the panel width (not shrink it).
        // total_width = 198 + 1 + 1 = 200. border_col = 1.
        // panel_width = 200 - 1 = 199. pct = 199*100/200 = 99.
        // right_width = 200*99/100 = 198. Panel width preserved!
        let right_width = (main_width as u32 * app.right_panel_percent as u32 / 100) as u16;
        assert_eq!(
            right_width, panel_content_width,
            "Panel width ({right_width}) should match original FullInspector width ({panel_content_width})"
        );

        // The drag offset should be non-zero: click at col 0, divider at col 2.
        assert_ne!(
            app.divider_drag_offset, 0,
            "Drag offset should compensate for click-to-border distance"
        );

        // Verify: first drag event to the same column should not change pct.
        let pct_before_drag = app.right_panel_percent;
        handle_mouse(&mut app, MouseEventKind::Drag(MouseButton::Left), 10, 0);
        assert_eq!(
            app.right_panel_percent, pct_before_drag,
            "First drag at same position should not change panel percent"
        );
    }

    #[test]
    fn general_divider_drag_start_does_not_snap() {
        // Regression: clicking the divider in normal split mode used to cause a
        // 1-2 column snap on the first drag event due to lossy percent↔width
        // integer-division round-trip.  The delta-based drag handler avoids this.
        let (mut app, _tmp) = build_test_app();

        let total_width: u16 = 157;
        let main_height: u16 = 40;

        // Test several percent values that are prone to rounding errors.
        for start_pct in [33u16, 37, 50, 67, 71, 25, 80] {
            let panel_width = (total_width as u32 * start_pct as u32 / 100) as u16;
            let graph_width = total_width - panel_width;

            app.right_panel_percent = start_pct;
            app.right_panel_visible = true;
            app.layout_mode = super::super::state::VizApp::layout_mode_for_percent(start_pct);
            app.last_graph_area = Rect {
                x: 0,
                y: 0,
                width: graph_width,
                height: main_height,
            };
            app.last_right_panel_area = Rect {
                x: graph_width,
                y: 0,
                width: panel_width,
                height: main_height,
            };
            // Divider grab zone: 3 columns centered on the panel border.
            app.last_divider_area = Rect {
                x: graph_width.saturating_sub(1),
                y: 0,
                width: 3,
                height: main_height,
            };
            // Clear areas that shouldn't be active.
            app.last_graph_scrollbar_area = Rect::default();
            app.last_panel_scrollbar_area = Rect::default();
            app.last_graph_hscrollbar_area = Rect::default();
            app.last_minimized_strip_area = Rect::default();
            app.last_fullscreen_restore_area = Rect::default();
            app.last_fullscreen_right_border_area = Rect::default();
            app.last_fullscreen_top_border_area = Rect::default();
            app.last_fullscreen_bottom_border_area = Rect::default();
            app.scrollbar_drag = None;

            // Click on the divider (at the panel border column).
            let click_col = graph_width;
            handle_mouse(
                &mut app,
                MouseEventKind::Down(MouseButton::Left),
                10,
                click_col,
            );
            assert_eq!(
                app.scrollbar_drag,
                Some(ScrollbarDragTarget::Divider),
                "pct={start_pct}: divider drag should start"
            );

            // First drag at the same column: percent must NOT change.
            let pct_before = app.right_panel_percent;
            handle_mouse(
                &mut app,
                MouseEventKind::Drag(MouseButton::Left),
                10,
                click_col,
            );
            assert_eq!(
                app.right_panel_percent, pct_before,
                "pct={start_pct}: first drag at same column should not change percent \
                 (was {pct_before}, got {})",
                app.right_panel_percent,
            );

            // Release.
            handle_mouse(
                &mut app,
                MouseEventKind::Up(MouseButton::Left),
                10,
                click_col,
            );
        }
    }

    // ── Horizontal divider drag tests (stacked mode) ──

    #[test]
    fn horizontal_divider_click_starts_drag() {
        let (mut app, _tmp) = build_test_app();
        let total_height: u16 = 40;
        let total_width: u16 = 70; // Narrow — will be stacked

        let start_pct: u16 = 35;
        let panel_height = (total_height as u32 * start_pct as u32 / 100) as u16;
        let graph_height = total_height - panel_height;

        app.right_panel_percent = start_pct;
        app.right_panel_visible = true;
        app.inspector_is_beside = false;
        app.layout_mode = super::super::state::VizApp::layout_mode_for_percent(start_pct);
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: total_width,
            height: graph_height,
        };
        app.last_right_panel_area = Rect {
            x: 0,
            y: graph_height,
            width: total_width,
            height: panel_height,
        };
        // Horizontal divider: 3 rows centered on the inspector top border.
        app.last_horizontal_divider_area = Rect {
            x: 0,
            y: graph_height.saturating_sub(1),
            width: total_width,
            height: 3,
        };
        // Clear vertical divider and other irrelevant areas.
        app.last_divider_area = Rect::default();
        app.last_graph_scrollbar_area = Rect::default();
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();
        app.last_minimized_strip_area = Rect::default();
        app.last_fullscreen_restore_area = Rect::default();
        app.last_fullscreen_right_border_area = Rect::default();
        app.last_fullscreen_top_border_area = Rect::default();
        app.last_fullscreen_bottom_border_area = Rect::default();
        app.scrollbar_drag = None;

        // Click on the horizontal divider.
        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            graph_height,
            10,
        );
        assert_eq!(
            app.scrollbar_drag,
            Some(ScrollbarDragTarget::HorizontalDivider),
            "clicking horizontal divider should start horizontal drag"
        );
        assert_eq!(app.divider_drag_start_pct, start_pct);
        assert_eq!(app.divider_drag_start_row, graph_height);
    }

    #[test]
    fn horizontal_divider_drag_up_grows_inspector() {
        let (mut app, _tmp) = build_test_app();
        let total_height: u16 = 40;
        let total_width: u16 = 70;

        let start_pct: u16 = 35;
        let panel_height = (total_height as u32 * start_pct as u32 / 100) as u16;
        let graph_height = total_height - panel_height;

        app.right_panel_percent = start_pct;
        app.right_panel_visible = true;
        app.inspector_is_beside = false;
        app.layout_mode = super::super::state::VizApp::layout_mode_for_percent(start_pct);
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: total_width,
            height: graph_height,
        };
        app.last_right_panel_area = Rect {
            x: 0,
            y: graph_height,
            width: total_width,
            height: panel_height,
        };
        app.last_horizontal_divider_area = Rect {
            x: 0,
            y: graph_height.saturating_sub(1),
            width: total_width,
            height: 3,
        };
        app.last_divider_area = Rect::default();
        app.last_graph_scrollbar_area = Rect::default();
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();
        app.last_minimized_strip_area = Rect::default();
        app.last_fullscreen_restore_area = Rect::default();
        app.last_fullscreen_right_border_area = Rect::default();
        app.last_fullscreen_top_border_area = Rect::default();
        app.last_fullscreen_bottom_border_area = Rect::default();
        app.scrollbar_drag = None;

        let click_row = graph_height;
        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            click_row,
            10,
        );

        // Drag UP by 4 rows: inspector should grow.
        let drag_row = click_row - 4;
        handle_mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            drag_row,
            10,
        );
        // delta = drag_row - click_row = -4, delta_pct = -4 * 100 / 40 = -10
        // pct = 35 - (-10) = 45
        assert!(
            app.right_panel_percent > start_pct,
            "dragging UP should grow inspector: got {} (expected > {start_pct})",
            app.right_panel_percent
        );
        assert_eq!(app.right_panel_percent, 45);
    }

    #[test]
    fn horizontal_divider_drag_down_shrinks_inspector() {
        let (mut app, _tmp) = build_test_app();
        let total_height: u16 = 40;
        let total_width: u16 = 70;

        let start_pct: u16 = 50;
        let panel_height = (total_height as u32 * start_pct as u32 / 100) as u16;
        let graph_height = total_height - panel_height;

        app.right_panel_percent = start_pct;
        app.right_panel_visible = true;
        app.inspector_is_beside = false;
        app.layout_mode = super::super::state::VizApp::layout_mode_for_percent(start_pct);
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: total_width,
            height: graph_height,
        };
        app.last_right_panel_area = Rect {
            x: 0,
            y: graph_height,
            width: total_width,
            height: panel_height,
        };
        app.last_horizontal_divider_area = Rect {
            x: 0,
            y: graph_height.saturating_sub(1),
            width: total_width,
            height: 3,
        };
        app.last_divider_area = Rect::default();
        app.last_graph_scrollbar_area = Rect::default();
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();
        app.last_minimized_strip_area = Rect::default();
        app.last_fullscreen_restore_area = Rect::default();
        app.last_fullscreen_right_border_area = Rect::default();
        app.last_fullscreen_top_border_area = Rect::default();
        app.last_fullscreen_bottom_border_area = Rect::default();
        app.scrollbar_drag = None;

        let click_row = graph_height;
        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            click_row,
            10,
        );

        // Drag DOWN by 4 rows: inspector should shrink.
        let drag_row = click_row + 4;
        handle_mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            drag_row,
            10,
        );
        // delta = 4, delta_pct = 4 * 100 / 40 = 10
        // pct = 50 - 10 = 40
        assert!(
            app.right_panel_percent < start_pct,
            "dragging DOWN should shrink inspector: got {} (expected < {start_pct})",
            app.right_panel_percent
        );
        assert_eq!(app.right_panel_percent, 40);
    }

    #[test]
    fn horizontal_divider_percent_clamped_at_extremes() {
        let (mut app, _tmp) = build_test_app();
        let total_height: u16 = 40;
        let total_width: u16 = 70;

        let start_pct: u16 = 10;
        let panel_height = (total_height as u32 * start_pct as u32 / 100) as u16;
        let graph_height = total_height - panel_height;

        app.right_panel_percent = start_pct;
        app.right_panel_visible = true;
        app.inspector_is_beside = false;
        app.layout_mode = super::super::state::VizApp::layout_mode_for_percent(start_pct);
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: total_width,
            height: graph_height,
        };
        app.last_right_panel_area = Rect {
            x: 0,
            y: graph_height,
            width: total_width,
            height: panel_height,
        };
        app.last_horizontal_divider_area = Rect {
            x: 0,
            y: graph_height.saturating_sub(1),
            width: total_width,
            height: 3,
        };
        app.last_divider_area = Rect::default();
        app.last_graph_scrollbar_area = Rect::default();
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();
        app.last_minimized_strip_area = Rect::default();
        app.last_fullscreen_restore_area = Rect::default();
        app.last_fullscreen_right_border_area = Rect::default();
        app.last_fullscreen_top_border_area = Rect::default();
        app.last_fullscreen_bottom_border_area = Rect::default();
        app.scrollbar_drag = None;

        let click_row = graph_height;
        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            click_row,
            10,
        );

        // Drag far DOWN past the bottom: percent should clamp at MIN_DRAG_PERCENT (10),
        // enforcing a minimum pane size so the inspector can't collapse to nothing.
        handle_mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            click_row + 50,
            10,
        );
        assert_eq!(
            app.right_panel_percent, MIN_DRAG_PERCENT as u16,
            "percent should clamp at MIN_DRAG_PERCENT when dragged far down"
        );

        // Release — should NOT finalize to Off because MIN_DRAG_PERCENT > 0.
        handle_mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            click_row + 50,
            10,
        );
        assert_ne!(
            app.layout_mode,
            super::super::state::LayoutMode::Off,
            "should not finalize to Off — minimum pane size enforced"
        );
    }

    #[test]
    fn horizontal_divider_drag_no_snap_on_same_row() {
        // Like the vertical divider no-snap test: first drag at same row should
        // not change percent.
        let (mut app, _tmp) = build_test_app();
        let total_height: u16 = 40;
        let total_width: u16 = 70;

        for start_pct in [33u16, 50, 67, 25, 80] {
            let panel_height = (total_height as u32 * start_pct as u32 / 100) as u16;
            let graph_height = total_height - panel_height;

            app.right_panel_percent = start_pct;
            app.right_panel_visible = true;
            app.inspector_is_beside = false;
            app.layout_mode = super::super::state::VizApp::layout_mode_for_percent(start_pct);
            app.last_graph_area = Rect {
                x: 0,
                y: 0,
                width: total_width,
                height: graph_height,
            };
            app.last_right_panel_area = Rect {
                x: 0,
                y: graph_height,
                width: total_width,
                height: panel_height,
            };
            app.last_horizontal_divider_area = Rect {
                x: 0,
                y: graph_height.saturating_sub(1),
                width: total_width,
                height: 3,
            };
            app.last_divider_area = Rect::default();
            app.last_graph_scrollbar_area = Rect::default();
            app.last_panel_scrollbar_area = Rect::default();
            app.last_graph_hscrollbar_area = Rect::default();
            app.last_minimized_strip_area = Rect::default();
            app.last_fullscreen_restore_area = Rect::default();
            app.last_fullscreen_right_border_area = Rect::default();
            app.last_fullscreen_top_border_area = Rect::default();
            app.last_fullscreen_bottom_border_area = Rect::default();
            app.scrollbar_drag = None;

            let click_row = graph_height;
            handle_mouse(
                &mut app,
                MouseEventKind::Down(MouseButton::Left),
                click_row,
                10,
            );
            let pct_before = app.right_panel_percent;
            handle_mouse(
                &mut app,
                MouseEventKind::Drag(MouseButton::Left),
                click_row,
                10,
            );
            assert_eq!(
                app.right_panel_percent, pct_before,
                "pct={start_pct}: drag at same row should not change percent"
            );

            handle_mouse(
                &mut app,
                MouseEventKind::Up(MouseButton::Left),
                click_row,
                10,
            );
        }
    }

    #[test]
    fn horizontal_divider_mouseup_clears_state() {
        let (mut app, _tmp) = build_test_app();
        let total_height: u16 = 40;
        let total_width: u16 = 70;

        let start_pct: u16 = 50;
        let panel_height = (total_height as u32 * start_pct as u32 / 100) as u16;
        let graph_height = total_height - panel_height;

        app.right_panel_percent = start_pct;
        app.right_panel_visible = true;
        app.inspector_is_beside = false;
        app.last_graph_area = Rect {
            x: 0,
            y: 0,
            width: total_width,
            height: graph_height,
        };
        app.last_right_panel_area = Rect {
            x: 0,
            y: graph_height,
            width: total_width,
            height: panel_height,
        };
        app.last_horizontal_divider_area = Rect {
            x: 0,
            y: graph_height.saturating_sub(1),
            width: total_width,
            height: 3,
        };
        app.last_divider_area = Rect::default();
        app.last_graph_scrollbar_area = Rect::default();
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();
        app.last_minimized_strip_area = Rect::default();
        app.last_fullscreen_restore_area = Rect::default();
        app.last_fullscreen_right_border_area = Rect::default();
        app.last_fullscreen_top_border_area = Rect::default();
        app.last_fullscreen_bottom_border_area = Rect::default();
        app.scrollbar_drag = None;

        handle_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            graph_height,
            10,
        );
        assert!(app.scrollbar_drag.is_some());

        handle_mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            graph_height,
            10,
        );
        assert_eq!(app.scrollbar_drag, None, "scrollbar_drag should be cleared");
        assert_eq!(
            app.divider_drag_start_row, 0,
            "drag_start_row should be reset"
        );
        assert_eq!(
            app.divider_drag_start_pct, 0,
            "drag_start_pct should be reset"
        );
    }

    // ── Inspector tab bar mouse click regression tests ──

    /// Helper: set up a test app for tab bar click tests with all conflicting
    /// hit areas cleared, so only the tab bar and horizontal divider matter.
    fn setup_tab_bar_test_app() -> (VizApp, tempfile::TempDir) {
        let (mut app, tmp) = build_test_app();
        app.last_graph_scrollbar_area = Rect::default();
        app.last_panel_scrollbar_area = Rect::default();
        app.last_graph_hscrollbar_area = Rect::default();
        app.last_coordinator_bar_area = Rect::default();
        app.last_minimized_strip_area = Rect::default();
        app.last_fullscreen_restore_area = Rect::default();
        app.last_fullscreen_right_border_area = Rect::default();
        app.last_fullscreen_top_border_area = Rect::default();
        app.last_fullscreen_bottom_border_area = Rect::default();
        app.last_service_badge_area = Rect::default();
        app.last_chat_input_area = Rect::default();
        app.last_message_input_area = Rect::default();
        app.last_chat_message_area = Rect::default();
        (app, tmp)
    }

    /// Regression test: clicking on the inspector tab bar should switch tabs,
    /// even in stacked (below) mode where the horizontal divider grab zone
    /// can overlap. Bug introduced in commit 77afe93.
    #[test]
    fn mouse_click_on_tab_bar_switches_tab_stacked_mode() {
        let (mut app, _tmp) = setup_tab_bar_test_app();
        app.right_panel_tab = RightPanelTab::Chat;
        app.focused_panel = FocusedPanel::Graph;
        app.inspector_is_beside = false;

        // Simulate stacked layout: graph on top, panel below.
        // Panel area starts at row 15, with border the inner area starts at row 16.
        app.last_graph_area = Rect::new(0, 0, 120, 15);
        app.last_right_panel_area = Rect::new(0, 15, 120, 15);
        app.last_tab_bar_area = Rect::new(1, 16, 118, 1);
        app.last_right_content_area = Rect::new(1, 17, 118, 13);
        app.last_divider_area = Rect::default();

        // Horizontal divider: 3 rows centered on the panel top border.
        // This overlaps with the tab bar at row 16!
        app.last_horizontal_divider_area = Rect::new(0, 14, 120, 3);

        // Click on "1:Detail" tab. In the Tabs widget with default padding,
        // "0:Chat" occupies cols 1..8 (padding + 6-char label + padding),
        // divider at col 9, then "1:Detail" starts at col 10.
        // Click at col 12 (within "1:Detail" label), row 16 (tab bar row).
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 16, 12);

        assert_eq!(
            app.right_panel_tab,
            RightPanelTab::Detail,
            "Clicking on the Detail tab in the tab bar should switch to Detail, \
             but the click was likely consumed by the horizontal divider handler"
        );
        assert_eq!(
            app.focused_panel,
            FocusedPanel::RightPanel,
            "Clicking on tab bar should focus the right panel"
        );
    }

    /// Verify that clicking each tab in the tab bar selects the correct tab.
    #[test]
    fn mouse_click_on_each_tab_in_bar() {
        let (mut app, _tmp) = setup_tab_bar_test_app();
        app.inspector_is_beside = false;

        // Layout: tab bar at row 16, inside panel starting at row 15.
        app.last_graph_area = Rect::new(0, 0, 120, 15);
        app.last_right_panel_area = Rect::new(0, 15, 120, 15);
        app.last_tab_bar_area = Rect::new(1, 16, 118, 1);
        app.last_right_content_area = Rect::new(1, 17, 118, 13);
        app.last_divider_area = Rect::default();
        app.last_horizontal_divider_area = Rect::new(0, 14, 120, 3);

        // Tab positions (relative to tab_bar_area.x = 1):
        // " 0:Chat " (8 cols) | " 1:Detail " (10 cols) | " 2:Log " (7 cols) | ...
        // Absolute columns:
        //   0:Chat => cols 1..8 (tab_bar.x + 0..7)
        //   divider at col 9
        //   1:Detail => cols 10..19
        //   divider at col 20
        //   2:Log => cols 21..27

        // Click on "0:Chat" (col 4, row 16)
        app.right_panel_tab = RightPanelTab::Log; // start on a different tab
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 16, 4);
        assert_eq!(
            app.right_panel_tab,
            RightPanelTab::Chat,
            "Click on Chat tab"
        );

        // Click on "1:Detail" (col 14, row 16)
        app.right_panel_tab = RightPanelTab::Chat;
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 16, 14);
        assert_eq!(
            app.right_panel_tab,
            RightPanelTab::Detail,
            "Click on Detail tab"
        );

        // Click on "2:Log" (col 24, row 16)
        app.right_panel_tab = RightPanelTab::Chat;
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 16, 24);
        assert_eq!(app.right_panel_tab, RightPanelTab::Log, "Click on Log tab");
    }

    /// Verify tab bar clicks still work in side-by-side mode (no horizontal divider).
    #[test]
    fn mouse_click_on_tab_bar_side_by_side_mode() {
        let (mut app, _tmp) = setup_tab_bar_test_app();
        app.right_panel_tab = RightPanelTab::Chat;
        app.focused_panel = FocusedPanel::Graph;
        app.inspector_is_beside = true;

        // Side-by-side: graph on left, panel on right.
        app.last_graph_area = Rect::new(0, 0, 60, 30);
        app.last_right_panel_area = Rect::new(60, 0, 60, 30);
        app.last_tab_bar_area = Rect::new(61, 1, 58, 1);
        app.last_right_content_area = Rect::new(61, 2, 58, 27);
        app.last_divider_area = Rect::new(59, 0, 3, 30);
        app.last_horizontal_divider_area = Rect::default();

        // Click on "1:Detail" tab area (col 72 relative to screen, row 1).
        handle_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 1, 72);
        assert_eq!(
            app.right_panel_tab,
            RightPanelTab::Detail,
            "Side-by-side: clicking Detail tab should switch tab"
        );
    }
}

#[cfg(test)]
mod drilldown_tests {
    use super::*;
    use crate::tui::viz_viewer::state::{
        DashboardAgentActivity, DashboardAgentRow, NavEntry, RightPanelTab,
    };

    fn setup_dashboard_app() -> (VizApp, tempfile::TempDir) {
        use crate::commands::viz::LayoutMode as VizLayoutMode;
        use crate::commands::viz::ascii::generate_ascii;
        use std::collections::{HashMap, HashSet};
        use workgraph::graph::{Node, Status, WorkGraph};
        use workgraph::parser::save_graph;
        use workgraph::test_helpers::make_task_with_status;

        let mut graph = WorkGraph::new();
        let t = make_task_with_status("test-task-1", "Test Task 1", Status::InProgress);
        graph.add_node(Node::Task(t));

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
            VizLayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.workgraph_dir = tmp.path().to_path_buf();
        app.dashboard.agent_rows.push(DashboardAgentRow {
            agent_id: "agent-99".into(),
            task_id: "test-task-1".into(),
            task_title: Some("Test Task 1".into()),
            activity: DashboardAgentActivity::Active,
            elapsed_secs: Some(10),
            model: Some("sonnet".into()),
            latest_snippet: None,
        });
        app.dashboard.selected_row = 0;
        app.right_panel_tab = RightPanelTab::Dashboard;
        app.focused_panel = FocusedPanel::RightPanel;
        (app, tmp)
    }

    #[test]
    fn dashboard_enter_pushes_nav_and_switches_to_output() {
        let (mut app, _tmp) = setup_dashboard_app();
        assert!(app.nav_stack.is_empty());
        app.nav_stack.push(NavEntry::Dashboard);
        app.output_pane.active_agent_id = Some("agent-99".into());
        app.right_panel_tab = RightPanelTab::Output;
        assert_eq!(app.right_panel_tab, RightPanelTab::Output);
        assert_eq!(app.output_pane.active_agent_id, Some("agent-99".into()));
        assert_eq!(app.nav_stack.len(), 1);
    }

    #[test]
    fn nav_pop_returns_to_dashboard() {
        let (mut app, _tmp) = setup_dashboard_app();
        app.nav_stack.push(NavEntry::Dashboard);
        app.right_panel_tab = RightPanelTab::Output;
        nav_stack_pop(&mut app);
        assert_eq!(app.right_panel_tab, RightPanelTab::Dashboard);
        assert!(app.nav_stack.is_empty());
    }

    #[test]
    fn nav_pop_empty_goes_to_graph() {
        let (mut app, _tmp) = setup_dashboard_app();
        assert!(app.nav_stack.is_empty());
        nav_stack_pop(&mut app);
        assert_eq!(app.focused_panel, FocusedPanel::Graph);
    }

    #[test]
    fn drilldown_dashboard_to_output_to_detail_and_back() {
        let (mut app, _tmp) = setup_dashboard_app();

        // Dashboard → Output
        app.nav_stack.push(NavEntry::Dashboard);
        app.output_pane.active_agent_id = Some("agent-99".into());
        app.right_panel_tab = RightPanelTab::Output;

        // Output → Detail
        app.nav_stack.push(NavEntry::AgentDetail {
            agent_id: "agent-99".into(),
        });
        app.load_hud_detail_for_task("test-task-1");
        app.right_panel_tab = RightPanelTab::Detail;

        assert_eq!(app.nav_stack.len(), 2);

        // Pop back to Output
        nav_stack_pop(&mut app);
        assert_eq!(app.right_panel_tab, RightPanelTab::Output);
        assert_eq!(app.output_pane.active_agent_id, Some("agent-99".into()));

        // Pop back to Dashboard
        nav_stack_pop(&mut app);
        assert_eq!(app.right_panel_tab, RightPanelTab::Dashboard);
        assert!(app.nav_stack.is_empty());
    }

    #[test]
    fn manual_tab_switch_clears_nav_stack() {
        let (mut app, _tmp) = setup_dashboard_app();
        app.nav_stack.push(NavEntry::Dashboard);
        app.nav_stack.push(NavEntry::AgentDetail {
            agent_id: "agent-99".into(),
        });
        assert_eq!(app.nav_stack.len(), 2);
        app.nav_stack.clear();
        app.right_panel_tab = RightPanelTab::Chat;
        assert!(app.nav_stack.is_empty());
    }
}
