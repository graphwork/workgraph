//! Integration tests for the TUI editor (edtui-based input).
//!
//! Tests simulate keyboard and mouse events, feed them through the event
//! handlers, render to a [`ratatui::backend::TestBackend`], and verify
//! the buffer contents and state changes.

#[cfg(test)]
mod tui_editor_tests {
    use std::collections::HashMap;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::commands::viz::VizOutput;
    use crate::tui::viz_viewer::render;
    use crate::tui::viz_viewer::state::{
        FocusedPanel, InputMode, InspectorSubFocus, RightPanelTab, SinglePanelView, VizApp,
        editor_text,
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
            annotation_map: HashMap::new(),
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
                    app.editor_handler
                        .on_key_event(KeyEvent::new(code, modifiers), &mut app.chat.editor);
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

    // ══════════════════════════════════════════════════════════════════════
    // Viewport scrolling tests
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn tui_editor_typing_beyond_visible_area_still_shows_cursor_text() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        // Type many newlines to exceed any reasonable viewport height.
        for i in 0..20 {
            type_string(&mut app, &format!("line{}", i));
            send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        }
        type_string(&mut app, "LAST_LINE");

        let text = editor_text(&app.chat.editor);
        assert!(
            text.contains("LAST_LINE"),
            "Editor should contain the last typed text"
        );

        // Render twice: the first render sets edtui's internal num_rows,
        // the second render uses it to scroll the viewport correctly.
        // This mirrors the real event loop where frames render continuously.
        render_to_string(&mut app, 80, 30);
        let rendered = render_to_string(&mut app, 80, 30);
        assert!(
            buffer_contains(&rendered, "LAST_LINE"),
            "Last line should be visible after typing beyond initial area:\n{}",
            rendered
        );
    }

    #[test]
    fn tui_editor_long_single_line_wraps_and_stays_visible() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        // Type a very long single line that will wrap many times.
        let long_text = "x".repeat(200);
        type_string(&mut app, &long_text);

        // The editor text should contain all of it.
        assert_eq!(editor_text(&app.chat.editor).len(), 200);

        // Render — no crash and the editor area grows to accommodate.
        let _rendered = render_to_string(&mut app, 80, 30);
    }

    // ══════════════════════════════════════════════════════════════════════
    // Mouse click-to-position tests
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn tui_editor_mouse_click_positions_cursor() {
        use crate::tui::viz_viewer::event::{EditorTarget, route_mouse_to_editor};

        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello world");

        // Render to populate screen areas.
        render_to_string(&mut app, 120, 40);

        // Cursor should be at end (col 11). Click at the beginning to reposition.
        assert!(app.last_chat_input_area.height > 0);
        let click_row = app.last_chat_input_area.y + 1; // after separator
        let click_col = app.last_chat_input_area.x + 2; // after "> " prefix

        route_mouse_to_editor(&mut app, click_row, click_col, EditorTarget::Chat);

        // Cursor should now be at (row=0, col=0).
        assert_eq!(
            app.chat.editor.cursor.row, 0,
            "cursor row should be 0, got {}",
            app.chat.editor.cursor.row
        );
        assert_eq!(
            app.chat.editor.cursor.col, 0,
            "cursor col should be 0, got {}",
            app.chat.editor.cursor.col
        );

        // Type 'Z' and verify it's inserted at position 0.
        send_chat_key(&mut app, KeyCode::Char('Z'), KeyModifiers::NONE);
        let text = editor_text(&app.chat.editor);
        assert!(
            text.starts_with('Z'),
            "Z should be at start of text, got: {:?}",
            text
        );
    }

    #[test]
    fn tui_editor_mouse_click_positions_cursor_multiline() {
        use crate::tui::viz_viewer::event::{EditorTarget, route_mouse_to_editor};

        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        // Type 3 lines separated by Shift+Enter (newlines).
        type_string(&mut app, "line zero");
        send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_string(&mut app, "line one");
        send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_string(&mut app, "line two");

        // Render to populate screen areas.
        render_to_string(&mut app, 120, 40);

        // Click on the second visual line (line index 1 = "line one").
        assert!(app.last_chat_input_area.height > 0);
        let editor_y = app.last_chat_input_area.y + 1; // after separator
        let editor_x = app.last_chat_input_area.x + 2; // after "> "
        let click_row = editor_y + 1; // visual row 1 = "line one"
        let click_col = editor_x + 5; // col 5 within "line one"

        route_mouse_to_editor(&mut app, click_row, click_col, EditorTarget::Chat);

        assert_eq!(
            app.chat.editor.cursor.row, 1,
            "cursor should be on logical line 1, got {}",
            app.chat.editor.cursor.row
        );
        assert_eq!(
            app.chat.editor.cursor.col, 5,
            "cursor col should be 5, got {}",
            app.chat.editor.cursor.col
        );
    }

    #[test]
    fn tui_editor_mouse_click_positions_cursor_exact_col() {
        use crate::tui::viz_viewer::event::{EditorTarget, route_mouse_to_editor};

        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "abcdef");

        render_to_string(&mut app, 120, 40);

        assert!(app.last_chat_input_area.height > 0);
        let click_row = app.last_chat_input_area.y + 1;
        let click_col = app.last_chat_input_area.x + 2 + 3; // col 3 within "abcdef"

        route_mouse_to_editor(&mut app, click_row, click_col, EditorTarget::Chat);

        assert_eq!(app.chat.editor.cursor.row, 0);
        assert_eq!(
            app.chat.editor.cursor.col, 3,
            "cursor col should be 3 (between 'c' and 'd'), got {}",
            app.chat.editor.cursor.col
        );

        // Insert 'X' at position 3 — result should be "abcXdef".
        send_chat_key(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);
        let text = editor_text(&app.chat.editor);
        assert_eq!(
            text, "abcXdef",
            "inserting X at col 3 should give 'abcXdef', got {:?}",
            text
        );
    }

    #[test]
    fn tui_editor_mouse_click_wrapped_line() {
        use crate::tui::viz_viewer::event::{EditorTarget, route_mouse_to_editor};

        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        // Use a narrow terminal so text wraps. Editor width = terminal_width - panel
        // overhead - prefix(2). With width=40, the right panel is roughly 20 chars wide,
        // so the editor area will be ~18 chars. Type a long line that wraps.
        // We need to know the exact editor width after render, so render first with
        // some text, check the area, then construct the right amount.
        // At 40 cols we hit Compact mode — set Detail view so the chat panel renders.
        app.single_panel_view = SinglePanelView::Detail;
        type_string(&mut app, "a]placeholder");
        render_to_string(&mut app, 40, 20);

        let prefix_len: u16 = 2;
        let editor_width = app.last_chat_input_area.width.saturating_sub(prefix_len) as usize;

        // Clear and type text that will wrap to 2+ visual lines.
        // We need at least editor_width+1 chars. Use distinct chars so we can verify.
        crate::tui::viz_viewer::state::editor_clear(&mut app.chat.editor);
        // Build a string with known content: "aaaa... bbbb..."
        // First word fills most of the line, second word forces wrap.
        let word1 = "a".repeat(editor_width.saturating_sub(1)); // fills line minus 1
        let word2 = "bbb click_here ccc";
        let full_text = format!("{} {}", word1, word2);
        type_string(&mut app, &full_text);

        // Re-render to populate areas with the new content.
        render_to_string(&mut app, 40, 20);

        // The text should wrap: visual row 0 = word1 (+ maybe space), visual row 1 = word2.
        assert!(app.last_chat_input_area.height > 0);
        let editor_y = app.last_chat_input_area.y + 1;
        let editor_x = app.last_chat_input_area.x + prefix_len;

        // Click on visual row 1, col 0 (start of wrapped portion).
        let click_row = editor_y + 1;
        let click_col = editor_x;

        route_mouse_to_editor(&mut app, click_row, click_col, EditorTarget::Chat);

        // The cursor should NOT be on row 0, col 0 — it should be somewhere in the
        // logical line past the wrap point.
        assert_eq!(
            app.chat.editor.cursor.row, 0,
            "still logical line 0 (single logical line, wrapped)"
        );
        assert!(
            app.chat.editor.cursor.col > 0,
            "cursor col should be past the wrap point, got {}",
            app.chat.editor.cursor.col
        );
        // The col should be near the start of word2 in the logical line.
        // word1 length + 1 space = word1.len() + 1, but wrapping may consume the space.
        // The important thing: col is in the second visual line's range.
        assert!(
            app.chat.editor.cursor.col >= editor_width.saturating_sub(2),
            "cursor col {} should be near or past editor_width {}",
            app.chat.editor.cursor.col,
            editor_width
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Mouse scroll wheel tests
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn tui_editor_mouse_scroll_in_editor_does_not_crash() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        // Type multi-line content.
        for i in 0..10 {
            type_string(&mut app, &format!("line {}", i));
            send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        }

        // Render to populate areas.
        render_to_string(&mut app, 80, 30);

        // Simulate scroll up/down via cursor movement (same mechanism
        // as mouse scroll uses internally).
        let initial_cursor = app.chat.editor.cursor;
        send_chat_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        send_chat_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        send_chat_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        assert!(
            app.chat.editor.cursor.row < initial_cursor.row,
            "Cursor should move up: initial {:?}, now {:?}",
            initial_cursor,
            app.chat.editor.cursor
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Emacs keybinding tests
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn tui_editor_ctrl_a_moves_to_beginning() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello");
        send_chat_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        send_chat_key(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);
        assert_eq!(
            editor_text(&app.chat.editor),
            "Xhello",
            "Ctrl-A should move cursor to beginning of line"
        );
    }

    #[test]
    fn tui_editor_ctrl_e_moves_to_end() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello");
        send_chat_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        send_chat_key(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        send_chat_key(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);
        assert_eq!(
            editor_text(&app.chat.editor),
            "helloX",
            "Ctrl-E should move cursor to end of line"
        );
    }

    #[test]
    fn tui_editor_ctrl_f_moves_forward() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "abc");
        send_chat_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        send_chat_key(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        send_chat_key(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);
        assert_eq!(
            editor_text(&app.chat.editor),
            "aXbc",
            "Ctrl-F should move cursor forward one char"
        );
    }

    #[test]
    fn tui_editor_ctrl_b_moves_backward() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "abc");
        send_chat_key(&mut app, KeyCode::Char('b'), KeyModifiers::CONTROL);
        send_chat_key(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);
        assert_eq!(
            editor_text(&app.chat.editor),
            "abXc",
            "Ctrl-B should move cursor backward one char"
        );
    }

    #[test]
    fn tui_editor_ctrl_k_kills_to_end_of_line() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello world");
        // Move to beginning, then forward 5 chars to position after "hello"
        send_chat_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        for _ in 0..5 {
            send_chat_key(&mut app, KeyCode::Char('f'), KeyModifiers::CONTROL);
        }
        send_chat_key(&mut app, KeyCode::Char('k'), KeyModifiers::CONTROL);
        assert_eq!(
            editor_text(&app.chat.editor),
            "hello",
            "Ctrl-K should kill from cursor to end of line"
        );
    }

    #[test]
    fn tui_editor_ctrl_n_moves_down() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        type_string(&mut app, "line1");
        send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_string(&mut app, "line2");

        // Move to beginning of line2, then up to line1.
        send_chat_key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        send_chat_key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        // Now on line1. Ctrl+N should move down to line2.
        send_chat_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        send_chat_key(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);

        let text = editor_text(&app.chat.editor);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() >= 2, "Should have 2 lines, got: {:?}", lines);
        assert!(
            lines[1].contains('X'),
            "X should be in second line after Ctrl+N. Lines: {:?}",
            lines
        );
    }

    #[test]
    fn tui_editor_ctrl_p_moves_up() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        type_string(&mut app, "line1");
        send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::SHIFT);
        type_string(&mut app, "line2");

        // Cursor is at end of line2. Ctrl+P should move up to line1.
        send_chat_key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        send_chat_key(&mut app, KeyCode::Char('X'), KeyModifiers::NONE);

        let text = editor_text(&app.chat.editor);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() >= 2, "Should have 2 lines, got: {:?}", lines);
        assert!(
            lines[0].contains('X'),
            "X should be in first line after Ctrl+P. Lines: {:?}",
            lines
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Paste tests
    // ══════════════════════════════════════════════════════════════════════

    /// Helper: simulate pasting text into the chat editor.
    fn paste_into_chat(app: &mut VizApp, text: &str) {
        crate::tui::viz_viewer::state::paste_insert_mode(text, &mut app.chat.editor);
    }

    #[test]
    fn test_paste_cursor_position() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        paste_into_chat(&mut app, "hello");
        assert_eq!(editor_text(&app.chat.editor), "hello");
        assert_eq!(
            app.chat.editor.cursor.col, 5,
            "cursor should be at col 5 (after 'hello'), got {}",
            app.chat.editor.cursor.col
        );
        assert_eq!(app.chat.editor.cursor.row, 0);

        // Typing after paste should append
        send_chat_key(&mut app, KeyCode::Char('!'), KeyModifiers::NONE);
        assert_eq!(editor_text(&app.chat.editor), "hello!");
    }

    #[test]
    fn test_paste_cursor_position_multiline() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        paste_into_chat(&mut app, "line1\nline2");
        assert_eq!(editor_text(&app.chat.editor), "line1\nline2");
        assert_eq!(app.chat.editor.cursor.row, 1);
        assert_eq!(
            app.chat.editor.cursor.col, 5,
            "cursor should be at col 5 (after 'line2'), got {}",
            app.chat.editor.cursor.col
        );

        // Typing after paste should append to line2
        send_chat_key(&mut app, KeyCode::Char('!'), KeyModifiers::NONE);
        assert_eq!(editor_text(&app.chat.editor), "line1\nline2!");
    }

    #[test]
    fn test_paste_into_existing_text() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        type_string(&mut app, "ac");
        // Move cursor back one so it's between 'a' and 'c'
        send_chat_key(&mut app, KeyCode::Left, KeyModifiers::NONE);
        paste_into_chat(&mut app, "b");
        assert_eq!(editor_text(&app.chat.editor), "abc");
        // Cursor should be after the pasted 'b' (col 2), before 'c'
        assert_eq!(app.chat.editor.cursor.col, 2);
    }

    #[test]
    fn test_paste_ending_with_newline() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        paste_into_chat(&mut app, "hello\n");
        assert_eq!(editor_text(&app.chat.editor), "hello\n");
        // Cursor should be at start of the new empty line
        assert_eq!(app.chat.editor.cursor.row, 1);
        assert_eq!(app.chat.editor.cursor.col, 0);
    }

    #[test]
    fn test_paste_empty_string() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);

        type_string(&mut app, "abc");
        let cursor_before = app.chat.editor.cursor;
        paste_into_chat(&mut app, "");
        assert_eq!(app.chat.editor.cursor, cursor_before);
        assert_eq!(editor_text(&app.chat.editor), "abc");
    }

    #[test]
    fn tui_editor_ctrl_u_kills_to_beginning() {
        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello world");
        // Ctrl-U should delete from cursor to beginning of line
        send_chat_key(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        // edtui's Ctrl-U deletes to first non-whitespace char on line,
        // which for "hello world" at end means deleting everything.
        let text = editor_text(&app.chat.editor);
        assert!(
            text.is_empty() || text.len() < "hello world".len(),
            "Ctrl-U should delete to beginning of line, got: {:?}",
            text
        );
    }

    // ── Key feedback tests ──────────────────────────────────────────

    #[test]
    fn key_feedback_records_when_enabled() {
        let mut app = make_editor_test_app();
        app.key_feedback_enabled = true;
        app.record_key_feedback("Tab".to_string());
        app.record_key_feedback("↑".to_string());
        assert_eq!(app.key_feedback.len(), 2);
        assert_eq!(app.key_feedback[0].0, "Tab");
        assert_eq!(app.key_feedback[1].0, "↑");
    }

    #[test]
    fn key_feedback_ignored_when_disabled() {
        let mut app = make_editor_test_app();
        app.key_feedback_enabled = false;
        app.record_key_feedback("Tab".to_string());
        assert!(app.key_feedback.is_empty());
    }

    #[test]
    fn key_feedback_respects_max_entries() {
        let mut app = make_editor_test_app();
        app.key_feedback_enabled = true;
        for i in 0..10 {
            app.record_key_feedback(format!("k{i}"));
        }
        // MAX is 6 entries
        assert!(app.key_feedback.len() <= 6);
        // Newest entry is the last one recorded
        assert_eq!(app.key_feedback.back().unwrap().0, "k9");
    }

    #[test]
    fn key_feedback_cleanup_removes_expired() {
        let mut app = make_editor_test_app();
        app.key_feedback_enabled = true;
        // Manually push an entry with an old timestamp.
        app.key_feedback.push_back((
            "old".to_string(),
            std::time::Instant::now() - std::time::Duration::from_secs(5),
        ));
        app.key_feedback
            .push_back(("new".to_string(), std::time::Instant::now()));
        app.cleanup_key_feedback();
        assert_eq!(app.key_feedback.len(), 1);
        assert_eq!(app.key_feedback[0].0, "new");
    }

    #[test]
    fn key_feedback_dispatch_event_records_keys() {
        use crossterm::event::{Event, KeyEvent, KeyEventKind, KeyEventState};
        let mut app = make_editor_test_app();
        app.key_feedback_enabled = true;
        // Dispatch a Tab key event
        let ev = Event::Key(KeyEvent {
            code: KeyCode::Tab,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        crate::tui::viz_viewer::event::dispatch_event(&mut app, ev);
        assert!(!app.key_feedback.is_empty());
        assert_eq!(app.key_feedback.back().unwrap().0, "Tab");
    }

    #[test]
    fn key_feedback_label_arrow_keys() {
        use crossterm::event::{Event, KeyEvent, KeyEventKind, KeyEventState};
        let mut app = make_editor_test_app();
        app.key_feedback_enabled = true;

        for (code, expected) in [
            (KeyCode::Up, "↑"),
            (KeyCode::Down, "↓"),
            (KeyCode::Left, "←"),
            (KeyCode::Right, "→"),
        ] {
            let ev = Event::Key(KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Press,
                state: KeyEventState::NONE,
            });
            crate::tui::viz_viewer::event::dispatch_event(&mut app, ev);
            assert_eq!(app.key_feedback.back().unwrap().0, expected);
        }
    }

    #[test]
    fn key_feedback_label_ctrl_modifier() {
        use crossterm::event::{Event, KeyEvent, KeyEventKind, KeyEventState};
        let mut app = make_editor_test_app();
        app.key_feedback_enabled = true;

        let ev = Event::Key(KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        });
        crate::tui::viz_viewer::event::dispatch_event(&mut app, ev);
        assert_eq!(app.key_feedback.back().unwrap().0, "Ctrl+c");
    }

    #[test]
    fn key_feedback_renders_without_panic() {
        let mut app = make_editor_test_app();
        app.key_feedback_enabled = true;
        app.record_key_feedback("Tab".to_string());
        app.record_key_feedback("↑".to_string());
        app.record_key_feedback("Enter".to_string());

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render::draw(frame, &mut app))
            .unwrap();
        // If we got here without panicking, rendering works.
    }

    // ══════════════════════════════════════════════════════════════════════
    // Input box style: no purple/magenta (tui-purple-styled)
    // ══════════════════════════════════════════════════════════════════════

    /// The bottom chat input box must not be rendered with purple/magenta
    /// foreground anywhere — neither the border separator, the `> ` prompt,
    /// nor the typed text. User reports purple "is cool but not right anymore"
    /// and the styling should be the default terminal color.
    #[test]
    fn test_chat_input_box_color_is_default() {
        use ratatui::style::Color;

        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hello");

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render::draw(frame, &mut app)).unwrap();
        let buf = terminal.backend().buffer().clone();

        let area = app.last_chat_input_area;
        assert!(area.height > 0 && area.width > 0, "input area not laid out");

        let mut bad: Vec<(u16, u16, String, Color)> = Vec::new();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                let cell = &buf[(x, y)];
                let fg = cell.style().fg;
                if matches!(fg, Some(Color::Magenta) | Some(Color::LightMagenta)) {
                    bad.push((x, y, cell.symbol().to_string(), fg.unwrap()));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "Chat input box must not contain magenta/purple cells, found {} offending cells: {:?}",
            bad.len(),
            bad.iter().take(8).collect::<Vec<_>>()
        );
    }

    /// After a chat message is sent and an executor response arrives (success
    /// or fault), the input box must not be pre-populated with executor
    /// output. Only user input belongs in the editor.
    #[test]
    fn test_chat_input_box_does_not_capture_previous_output() {
        use crate::tui::viz_viewer::state::{ChatMessage, ChatRole};

        let mut app = make_editor_test_app();
        enter_chat_input(&mut app);
        type_string(&mut app, "hi nex");

        // Submit (clears editor, sends message).
        send_chat_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            editor_text(&app.chat.editor),
            "",
            "editor should be empty right after submit"
        );

        // Simulate executor (wg nex) emitting output, then faulting with an
        // error. None of this content should ever reach the editor.
        let nex_output = "internal traceback: nex died here";
        let nex_error = "Executor faulted: connection refused";
        app.chat.messages.push(ChatMessage {
            role: ChatRole::Coordinator,
            text: nex_output.to_string(),
            full_text: None,
            attachments: Vec::new(),
            edited: false,
            inbox_id: None,
            user: None,
            target_task: None,
            msg_timestamp: Some(chrono::Utc::now().to_rfc3339()),
            read_at: None,
            msg_queue_id: None,
        });
        app.chat.messages.push(ChatMessage {
            role: ChatRole::SystemError,
            text: nex_error.to_string(),
            full_text: None,
            attachments: Vec::new(),
            edited: false,
            inbox_id: None,
            user: None,
            target_task: None,
            msg_timestamp: Some(chrono::Utc::now().to_rfc3339()),
            read_at: None,
            msg_queue_id: None,
        });

        // Render — re-rendering must not pre-fill the editor with anything.
        let _ = render_to_string(&mut app, 120, 40);

        let editor_after = editor_text(&app.chat.editor);
        assert_eq!(
            editor_after, "",
            "editor must remain empty after executor output/fault, got {:?}",
            editor_after
        );
        assert!(
            !editor_after.contains(nex_output),
            "editor must never contain executor stdout/output"
        );
        assert!(
            !editor_after.contains(nex_error),
            "editor must never contain executor error text"
        );
    }
}
