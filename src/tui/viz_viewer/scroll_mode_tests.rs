/// Tests for the Ctrl+] scroll mode on the chat PTY pane.
///
/// These tests exercise: entering scroll mode, navigation keys, key-swallowing,
/// exit keys, and automatic exit on pane-switch.
#[cfg(test)]
mod scroll_mode_tests {
    use std::collections::HashMap;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::commands::viz::VizOutput;
    use crate::tui::pty_pane::PtyPane;
    use crate::tui::viz_viewer::event::dispatch_event;
    use crate::tui::viz_viewer::state::{FocusedPanel, InputMode, RightPanelTab, VizApp};

    fn make_viz_output() -> VizOutput {
        VizOutput {
            text: String::from("(empty graph)"),
            node_line_map: HashMap::new(),
            task_order: Vec::new(),
            forward_edges: HashMap::new(),
            reverse_edges: HashMap::new(),
            char_edge_map: HashMap::new(),
            cycle_members: HashMap::new(),
            annotation_map: HashMap::new(),
        }
    }

    /// Build a minimal VizApp in PTY-active mode (chat tab, right panel focused, stdin forwarded).
    fn make_pty_app() -> VizApp {
        let viz = make_viz_output();
        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.right_panel_visible = true;
        app.right_panel_tab = RightPanelTab::Chat;
        app.focused_panel = FocusedPanel::RightPanel;
        app.chat_pty_mode = true;
        app.chat_pty_forwards_stdin = true;
        app.mouse_enabled = true;
        app
    }

    /// Insert a live PTY pane (spawns /bin/cat) under the active coordinator task_id.
    fn insert_test_pane(app: &mut VizApp) -> String {
        let task_id = workgraph::chat_id::format_chat_task_id(app.active_coordinator_id);
        let pane =
            PtyPane::spawn("/bin/cat", &[], &[], 24, 80).expect("spawn /bin/cat for test");
        app.task_panes.insert(task_id.clone(), pane);
        task_id
    }

    fn send_key(app: &mut VizApp, code: KeyCode, modifiers: KeyModifiers) {
        use crossterm::event::{Event, KeyEventKind};
        let ev = Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        });
        dispatch_event(app, ev);
    }

    fn ctrl(ch: char) -> (KeyCode, KeyModifiers) {
        (KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn bare(code: KeyCode) -> (KeyCode, KeyModifiers) {
        (code, KeyModifiers::NONE)
    }

    // ── Tests ─────────────────────────────────────────────────────────────

    #[test]
    fn enter_scroll_mode_via_ctrl_bracket_changes_input_mode() {
        let mut app = make_pty_app();
        let task_id = insert_test_pane(&mut app);
        assert_eq!(app.input_mode, InputMode::Normal);

        let (code, mods) = ctrl(']');
        send_key(&mut app, code, mods);

        assert_eq!(
            app.input_mode,
            InputMode::ScrollMode {
                task_id: task_id.clone()
            }
        );
    }

    #[test]
    fn scroll_mode_esc_exits_back_to_normal() {
        let mut app = make_pty_app();
        let task_id = insert_test_pane(&mut app);

        // Enter scroll mode directly
        app.input_mode = InputMode::ScrollMode {
            task_id: task_id.clone(),
        };

        let (code, mods) = bare(KeyCode::Esc);
        send_key(&mut app, code, mods);

        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn scroll_mode_q_exits_back_to_normal() {
        let mut app = make_pty_app();
        let task_id = insert_test_pane(&mut app);

        app.input_mode = InputMode::ScrollMode {
            task_id: task_id.clone(),
        };

        send_key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);

        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn scroll_mode_ctrl_bracket_toggles_back_to_normal() {
        let mut app = make_pty_app();
        let task_id = insert_test_pane(&mut app);

        app.input_mode = InputMode::ScrollMode {
            task_id: task_id.clone(),
        };

        let (code, mods) = ctrl(']');
        send_key(&mut app, code, mods);

        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn scroll_mode_pageup_scrolls_back() {
        let mut app = make_pty_app();
        let task_id = insert_test_pane(&mut app);

        // Feed enough output to create some scrollback.
        if let Some(pane) = app.task_panes.get_mut(&task_id) {
            for _ in 0..50 {
                let _ = pane.send_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            pane.scroll_to_bottom(); // ensure at bottom
        }

        app.input_mode = InputMode::ScrollMode {
            task_id: task_id.clone(),
        };
        // Set a viewport height so the page calculation is non-zero.
        app.last_right_content_area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };

        send_key(&mut app, KeyCode::PageUp, KeyModifiers::NONE);

        // Mode stays active after a nav key.
        assert!(matches!(
            app.input_mode,
            InputMode::ScrollMode { .. }
        ));
        // Pane should now be scrolled back (auto_follow disabled).
        let scrolled_back = app
            .task_panes
            .get(&task_id)
            .map(|p| p.is_scrolled_back())
            .unwrap_or(false);
        assert!(scrolled_back, "PageUp should scroll back into history");
    }

    #[test]
    fn scroll_mode_end_key_returns_to_live() {
        let mut app = make_pty_app();
        let task_id = insert_test_pane(&mut app);

        // Scroll up first, then End should return to live output.
        if let Some(pane) = app.task_panes.get_mut(&task_id) {
            pane.scroll_to_top();
        }

        app.input_mode = InputMode::ScrollMode {
            task_id: task_id.clone(),
        };
        app.last_right_content_area = ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 24,
        };

        send_key(&mut app, KeyCode::End, KeyModifiers::NONE);

        let scrolled_back = app
            .task_panes
            .get(&task_id)
            .map(|p| p.is_scrolled_back())
            .unwrap_or(true);
        assert!(!scrolled_back, "End key should return to live output");
    }

    #[test]
    fn scroll_mode_swallows_letter_key_does_not_send_to_pty() {
        let mut app = make_pty_app();
        let task_id = insert_test_pane(&mut app);
        std::thread::sleep(std::time::Duration::from_millis(50));

        let bytes_before = app
            .task_panes
            .get(&task_id)
            .map(|p| p.bytes_processed())
            .unwrap_or(0);

        app.input_mode = InputMode::ScrollMode {
            task_id: task_id.clone(),
        };

        // Send 'h' — should be swallowed, NOT forwarded.
        send_key(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
        std::thread::sleep(std::time::Duration::from_millis(50));

        let bytes_after = app
            .task_panes
            .get(&task_id)
            .map(|p| p.bytes_processed())
            .unwrap_or(0);

        assert!(
            bytes_after == bytes_before,
            "Scroll mode should swallow 'h' — expected bytes_processed to remain at {bytes_before}, got {bytes_after}"
        );
        // Mode should still be scroll mode.
        assert!(matches!(app.input_mode, InputMode::ScrollMode { .. }));
    }

    #[test]
    fn passthrough_forwards_letter_key_to_pty() {
        // Control: in passthrough mode, 'h' IS forwarded.
        let mut app = make_pty_app();
        let task_id = insert_test_pane(&mut app);
        std::thread::sleep(std::time::Duration::from_millis(50));

        let bytes_before = app
            .task_panes
            .get(&task_id)
            .map(|p| p.bytes_processed())
            .unwrap_or(0);

        // NOT in scroll mode — passthrough.
        send_key(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
        std::thread::sleep(std::time::Duration::from_millis(50));

        let bytes_after = app
            .task_panes
            .get(&task_id)
            .map(|p| p.bytes_processed())
            .unwrap_or(0);

        assert!(
            bytes_after > bytes_before,
            "Passthrough should forward 'h' to PTY — expected bytes_processed to increase from {bytes_before}, got {bytes_after}"
        );
    }

    #[test]
    fn pane_switch_exits_scroll_mode() {
        let mut app = make_pty_app();
        let task_id = insert_test_pane(&mut app);

        app.input_mode = InputMode::ScrollMode {
            task_id: task_id.clone(),
        };

        // Simulate pane switch: focus away from right panel.
        app.focused_panel = FocusedPanel::Graph;

        // Send any key — the scroll mode handler will detect still_valid=false and exit.
        send_key(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);

        assert_eq!(
            app.input_mode,
            InputMode::Normal,
            "Scroll mode should exit when pane focus shifts away"
        );
    }

    #[test]
    fn switch_coordinator_exits_scroll_mode() {
        let mut app = make_pty_app();
        let task_id = insert_test_pane(&mut app);

        app.input_mode = InputMode::ScrollMode {
            task_id: task_id.clone(),
        };
        // Ensure there's at least one other tab to switch to.
        app.active_tabs = vec![0, 1];

        // switch_coordinator auto-exits scroll mode.
        app.switch_coordinator(1);

        assert_eq!(app.input_mode, InputMode::Normal);
    }
}
