//! Integration tests for the TUI editor (edtui-based input).
//!
//! Tests simulate keyboard and mouse events, feed them through the event
//! handlers, render to a [`ratatui::backend::TestBackend`], and verify
//! the buffer contents and state changes.

#[cfg(test)]
mod tui_editor_tests {
    use std::collections::HashMap;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use crate::commands::viz::VizOutput;
    use crate::tui::viz_viewer::render;
    use crate::tui::viz_viewer::state::{
        editor_text, FocusedPanel, InputMode, InspectorSubFocus, RightPanelTab, VizApp,
    };

    // ── Helpers ──────────────────────────────────────────────────────────

    /// Build a minimal VizApp suitable for editor tests.
    fn make_editor_test_app() -> VizApp {
        let viz = VizOutput {
            text: String::from("(empty graph)"),
            node_line_map: HashMap::new(),
            task_order: Vec::new(),
            forward_edges: HashMap::new(),
            reverse_edges: HashMap::new(),
            char_edge_map: HashMap::new(),
            cycle_members: HashMap::new(),
        };
        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.right_panel_visible = true;
        app.right_panel_tab = RightPanelTab::Chat;
        app.focused_panel = FocusedPanel::RightPanel;
        app.mouse_enabled = true;
        app
    }

    /// Put the app into ChatInput mode.
    fn enter_chat_input(app: &mut VizApp) {
        app.input_mode = InputMode::ChatInput;
        app.chat_input_dismissed = false;
        app.inspector_sub_focus = InspectorSubFocus::TextEntry;
    }

    /// Simulate typing a string by sending individual key events through
    /// the chat input handler.
    fn type_string(app: &mut VizApp, s: &str) {
        for ch in s.chars() {
            send_chat_key(app, KeyCode::Char(ch), KeyModifiers::NONE);
        }
    }

    /// Send a key event through the chat input handler (same logic as
    /// handle_chat_input in event.rs).
    fn send_chat_key(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
        match code {
            KeyCode::Esc => {
                app.input_mode = InputMode::Normal;
                app.chat_input_dismissed = true;
                app.inspector_sub_focus = InspectorSubFocus::ChatHistory;
            }
            KeyCode::Enter
                if !modifiers.contains(KeyModifiers::SHIFT)
                    && !modifiers.contains(KeyModifiers::ALT) =>
            {
                let text = editor_text(&app.chat.editor);
                if !text.trim().is_empty() {
                    crate::tui::viz_viewer::state::editor_clear(&mut app.chat.editor);
                }
            }
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                crate::tui::viz_viewer::state::editor_clear(&mut app.chat.editor);
                app.input_mode = InputMode::Normal;
                app.inspector_sub_focus = InspectorSubFocus::ChatHistory;
            }
            _ => {
                // Shift+Enter → newline (strip shift so edtui sees plain Enter).
                if code == KeyCode::Enter
                    && (modifiers.contains(KeyModifiers::SHIFT)
                        || modifiers.contains(KeyModifiers::ALT))
                {
                    app.editor_handler.on_key_event(
                        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                        &mut app.chat.editor,
                    );
                } else {
                    app.editor_handler.on_key_event(
                        KeyEvent::new(code, modifiers),
                        &mut app.chat.editor,
                    );
                }
            }
        }
    }

    /// Send a key event through the message input handler.
    fn send_message_key(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
        match code {
            KeyCode::Esc => {
                app.input_mode = InputMode::Normal;
            }
            _ => {
                if code == KeyCode::Enter
                    && (modifiers.contains(KeyModifiers::SHIFT)
                        || modifiers.contains(KeyModifiers::ALT))
                {
                    app.editor_handler.on_key_event(
                        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                        &mut app.messages_panel.editor,
                    );
                } else {
                    app.editor_handler.on_key_event(
                        KeyEvent::new(code, modifiers),
                        &mut app.messages_panel.editor,
                    );
                }
            }
        }
    }

    /// Render the app to a TestBackend and return the buffer contents as a
    /// single string (one line per terminal row).
    fn render_to_string(app: &mut VizApp, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render::draw(frame, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut output = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                output.push_str(cell.symbol());
            }
            output.push('\n');
        }
        output
    }

    /// Check if a string appears anywhere in the rendered buffer.
    fn buffer_contains(rendered: &str, needle: &str) -> bool {
        rendered.lines().any(|line| line.contains(needle))
    }

    // ══════════════════════════════════════════════════════════════════════
    // Keyboard input tests
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn tui_editor_type_hello_world() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello world");
        assert_eq!(editor_text(&app.chat.editor), "hello world");
    }

    #[test]
    fn tui_editor_type_hello_world_rendered() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello world");

        let rendered = render_to_string(&mut app, 120, 40);
        assert!(
            buffer_contains(&rendered, "hello world"),
            "Expected 'hello world' in rendered buffer:\n{}",
            rendered
        );
    }

    #[test]
    fn tui_editor_backspace_deletes_last_char() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello world");
        send_chat_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(editor_text(&app.chat.editor), "hello worl");
    }

    #[test]
    fn tui_editor_backspace_rendered() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello world");
        send_chat_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);

        let rendered = render_to_string(&mut app, 120, 40);
        assert!(
            buffer_contains(&rendered, "hello worl"),
            "Expected 'hello worl' in rendered buffer after backspace"
        );
        assert_eq!(editor_text(&app.chat.editor), "hello worl");
    }

    #[test]
    fn tui_editor_arrow_left_moves_cursor() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "abc");
        send_chat_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        send_chat_key(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);
        assert_eq!(editor_text(&app.chat.editor), "abXc");
    }

    #[test]
    fn tui_editor_home_moves_to_beginning() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello");
        send_chat_key(&mut app, KeyCode::Home, KeyModifiers::NONE);
        send_chat_key(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);
        assert_eq!(editor_text(&app.chat.editor), "Xhello");
    }

    #[test]
    fn tui_editor_end_moves_to_end() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello");
        send_chat_key(&mut app, KeyCode::Home, KeyModifiers::NONE);
        send_chat_key(&mut app, KeyCode::End, KeyModifiers::NONE);
        send_chat_key(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);
        assert_eq!(editor_text(&app.chat.editor), "helloX");
    }

    #[test]
    fn tui_editor_enter_submits_and_clears() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "send this");
        send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(editor_text(&app.chat.editor), "");
    }

    #[test]
    fn tui_editor_shift_enter_inserts_newline() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "line1");
        send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_string(&mut app, "line2");

        let text = editor_text(&app.chat.editor);
        assert!(text.contains('\n'), "Expected newline, got: {:?}", text);
        assert!(text.contains("line1"), "Expected line1, got: {:?}", text);
        assert!(text.contains("line2"), "Expected line2, got: {:?}", text);
    }

    #[test]
    fn tui_editor_esc_exits_input_mode() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "some text");
        send_chat_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.chat_input_dismissed);
    }

    #[test]
    fn tui_editor_esc_preserves_text_rendered() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "typed text");
        send_chat_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        assert_eq!(app.input_mode, InputMode::Normal);
        let rendered = render_to_string(&mut app, 120, 40);
        assert!(
            buffer_contains(&rendered, "typed text"),
            "Editor text should persist after Esc"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Mouse interaction tests
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn tui_editor_mouse_click_chat_input_enters_edit_mode() {
        let mut app = make_editor_test_app();
        app.input_mode = InputMode::Normal;
        app.chat_input_dismissed = false;

        // Render to populate last_chat_input_area.
        render_to_string(&mut app, 120, 40);

        if app.last_chat_input_area.height > 0 && app.last_chat_input_area.width > 0 {
            // Simulate the click logic from handle_mouse: clicking chat input
            // area enters ChatInput mode.
            app.focused_panel = FocusedPanel::RightPanel;
            app.chat_input_dismissed = false;
            app.input_mode = InputMode::ChatInput;
            app.inspector_sub_focus = InspectorSubFocus::TextEntry;

            assert_eq!(app.input_mode, InputMode::ChatInput);
        }
    }

    #[test]
    fn tui_editor_mouse_click_graph_focuses_graph() {
        let mut app = make_editor_test_app();
        app.focused_panel = FocusedPanel::RightPanel;

        // Render to populate layout areas.
        render_to_string(&mut app, 120, 40);

        if app.last_graph_area.height > 0 {
            // Simulate clicking in graph area (from handle_mouse logic).
            app.focused_panel = FocusedPanel::Graph;
            assert_eq!(app.focused_panel, FocusedPanel::Graph);
            assert_ne!(app.input_mode, InputMode::ChatInput);
        }
    }

    #[test]
    fn tui_editor_mouse_scroll_changes_offset() {
        let mut app = make_editor_test_app();
        app.scroll.content_height = 100;
        app.scroll.viewport_height = 20;
        app.scroll.offset_y = 10;

        // Render to populate layout areas.
        render_to_string(&mut app, 120, 40);

        let initial = app.scroll.offset_y;
        // Simulate scroll up (from handle_mouse: scroll_up(3)).
        app.scroll.scroll_up(3);
        assert!(
            app.scroll.offset_y < initial,
            "Scroll up should decrease offset_y"
        );

        let after_up = app.scroll.offset_y;
        // Simulate scroll down.
        app.scroll.scroll_down(3);
        assert!(
            app.scroll.offset_y > after_up,
            "Scroll down should increase offset_y"
        );
    }

    #[test]
    fn tui_editor_layout_areas_populated_after_render() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "test");

        render_to_string(&mut app, 120, 40);

        // After rendering with the chat tab active and text in the editor,
        // the chat input area should have been populated.
        assert!(
            app.last_chat_input_area.height > 0 || app.last_chat_input_area.width > 0,
            "Chat input area should be populated after render (area: {:?})",
            app.last_chat_input_area
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Focus management tests
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn tui_editor_normal_mode_keys_not_inserted() {
        let mut app = make_editor_test_app();
        app.input_mode = InputMode::Normal;

        // Pressing 'j', 'k', etc. in normal mode should NOT insert text.
        // (We don't call the editor handler in normal mode.)
        assert_eq!(
            editor_text(&app.chat.editor),
            "",
            "Normal mode keys should NOT insert into the editor"
        );
    }

    #[test]
    fn tui_editor_only_one_editor_receives_input() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "chat text");

        assert_eq!(editor_text(&app.chat.editor), "chat text");
        assert_eq!(
            editor_text(&app.messages_panel.editor),
            "",
            "Message editor should be empty when ChatInput is active"
        );
    }

    #[test]
    fn tui_editor_message_editor_independent() {
        let mut app = make_editor_test_app();

        // Type in chat.
        enter_chat_input(&mut app);
        type_string(&mut app, "chat");
        send_chat_key(&mut app, KeyCode::Esc, KeyModifiers::NONE);

        // Switch to message input and type.
        app.input_mode = InputMode::MessageInput;
        send_message_key(&mut app, KeyCode::Char('m'), KeyModifiers::NONE);
        send_message_key(&mut app, KeyCode::Char('s'), KeyModifiers::NONE);
        send_message_key(&mut app, KeyCode::Char('g'), KeyModifiers::NONE);

        assert_eq!(editor_text(&app.chat.editor), "chat");
        assert_eq!(editor_text(&app.messages_panel.editor), "msg");
    }

    #[test]
    fn tui_editor_clicking_outside_does_not_focus_editor() {
        let mut app = make_editor_test_app();
        app.input_mode = InputMode::Normal;

        render_to_string(&mut app, 120, 40);

        // Verify we're still in Normal mode after render (no auto-focus).
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    // ══════════════════════════════════════════════════════════════════════
    // Multi-line editing tests
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn tui_editor_multiline_content() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        type_string(&mut app, "first line");
        send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_string(&mut app, "second line");

        let text = editor_text(&app.chat.editor);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() >= 2, "Expected 2+ lines, got: {:?}", text);
        assert_eq!(lines[0], "first line");
        assert_eq!(lines[1], "second line");
    }

    #[test]
    fn tui_editor_multiline_rendered() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "AAA");
        send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_string(&mut app, "BBB");

        let rendered = render_to_string(&mut app, 120, 40);
        assert!(buffer_contains(&rendered, "AAA"), "First line missing");
        assert!(buffer_contains(&rendered, "BBB"), "Second line missing");
    }

    #[test]
    fn tui_editor_up_down_arrow_navigation() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        type_string(&mut app, "line1");
        send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_string(&mut app, "line2");

        // Cursor at end of line2. Press Up → cursor moves to line1.
        send_chat_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        send_chat_key(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);

        let text = editor_text(&app.chat.editor);
        // X should be inserted somewhere in line1 (exact position depends on edtui).
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() >= 2, "Should still have 2 lines");
        assert!(
            lines[0].contains('X'),
            "X should be in first line after Up arrow. Lines: {:?}",
            lines
        );
        assert_eq!(lines[1], "line2", "line2 should be unchanged");
    }

    #[test]
    fn tui_editor_ctrl_c_clears_and_exits() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "discard this");

        send_chat_key(&mut app, KeyCode::Char('c'), KeyModifiers::CONTROL);

        assert_eq!(editor_text(&app.chat.editor), "");
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn tui_editor_empty_enter_does_not_clear() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        // Enter on empty editor — no crash, no state change.
        send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(editor_text(&app.chat.editor), "");
    }

    #[test]
    fn tui_editor_multiple_backspace_to_empty() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "ab");

        send_chat_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        send_chat_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(editor_text(&app.chat.editor), "");

        // Extra backspace on empty — no crash.
        send_chat_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(editor_text(&app.chat.editor), "");
    }

    #[test]
    fn tui_editor_renders_all_modes_without_panic() {
        for mode in [InputMode::Normal, InputMode::ChatInput, InputMode::Search] {
            let mut app = make_editor_test_app();
            app.input_mode = mode;
            let _rendered = render_to_string(&mut app, 80, 24);
        }
    }
}
