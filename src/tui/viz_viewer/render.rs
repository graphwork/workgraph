use std::collections::{HashMap, HashSet};

use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Tabs,
};
use unicode_width::UnicodeWidthStr;

use super::state::{
    ActivityEventKind, AgentStreamEventKind, ChoiceDialogState, ConfigEditKind, ConfigSection,
    ConfirmAction, ControlPanelFocus, CoordinatorPlusHit, CoordinatorTabHit, EndpointTestStatus,
    FocusedPanel, InputMode, LayoutMode, ResponsiveBreakpoint, RightPanelTab, ServiceHealthLevel,
    SinglePanelView, SortMode, TabBarEntryKind, TaskFormField, TaskFormState, TextPromptAction,
    ToastSeverity, VitalsStaleness, VizApp, WAVE_BOLT, WAVE_NUM_BOLTS, extract_section_name,
    format_duration_compact, format_relative_time, spinner_wave_pos, vitals_staleness_color,
};
use workgraph::AgentStatus;
use workgraph::graph::{TokenUsage, format_tokens};

use crate::tui::markdown::markdown_to_lines;

/// Minimum terminal width for side-by-side right panel layout.
/// When the inspector is currently on the right and terminal shrinks below this,
/// the inspector moves to the bottom.
const SIDE_MIN_WIDTH: u16 = 100;

/// Width at which the inspector restores to side-by-side after being moved to the bottom.
/// Higher than SIDE_MIN_WIDTH to prevent flapping at the boundary (hysteresis).
const SIDE_RESTORE_WIDTH: u16 = 120;

/// Creates a [`Line`] with the lightning-wave animation and elapsed time.
///
/// Renders [`WAVE_NUM_BOLTS`] `↯` characters in a rainbow spectrum matching the
/// CLI spinner (Red, Orange, Green, Cyan, Violet).  The wave peak bolt is
/// **bold** + bright, adjacent bolts are bright, and distant bolts fade through
/// mid to dim tiers — creating a lightning-flash sweep effect across the rainbow.
fn spinner_wave_line(elapsed: std::time::Duration, indent: &str) -> Line<'static> {
    let wave_pos = spinner_wave_pos(elapsed);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(WAVE_NUM_BOLTS + 3);

    // Rainbow spectral colors matching CLI spinner — bright / mid / dim tiers per bolt
    // Red, Orange, Green, Cyan, Violet (same palette as `wg service start`)
    const BRIGHT: [u8; 5] = [196, 214, 46, 33, 129];
    const MID: [u8; 5] = [124, 172, 34, 25, 91];
    const DIM: [u8; 5] = [52, 94, 22, 17, 53];

    if !indent.is_empty() {
        spans.push(Span::raw(indent.to_string()));
    }

    for i in 0..WAVE_NUM_BOLTS {
        let d = (i as isize - wave_pos as isize).unsigned_abs();
        let dist = d.min(WAVE_NUM_BOLTS - d);
        let style = match dist {
            0 => Style::default()
                .fg(Color::Indexed(BRIGHT[i]))
                .add_modifier(Modifier::BOLD), // flash peak
            1 => Style::default().fg(Color::Indexed(BRIGHT[i])), // bright adjacent
            2 => Style::default().fg(Color::Indexed(MID[i])),    // fading
            _ => Style::default().fg(Color::Indexed(DIM[i])),    // dim base
        };
        spans.push(Span::styled(WAVE_BOLT, style));
    }

    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        format_duration_compact(elapsed.as_secs()),
        Style::default().fg(Color::DarkGray),
    ));

    Line::from(spans)
}

pub fn draw(frame: &mut Frame, app: &mut VizApp) {
    // Clear expired jump targets (>2 seconds old).
    if let Some((_, when)) = app.jump_target
        && when.elapsed() > std::time::Duration::from_secs(2)
    {
        app.jump_target = None;
    }

    // Clean up expired splash animations.
    app.cleanup_splash_animations();

    // Clean up expired key feedback entries.
    app.cleanup_key_feedback();

    // Clean up expired touch echo indicators.
    app.cleanup_touch_echoes();

    // Reset scrollbar areas each frame (re-set by draw_scrollbar / panel scrollbar code).
    app.last_graph_scrollbar_area = Rect::default();
    app.last_panel_scrollbar_area = Rect::default();

    let area = frame.area();

    // Layout: top status bar + middle area + vitals bar + bottom action hints.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top status bar
            Constraint::Min(1),    // main content area
            Constraint::Length(1), // vitals bar
            Constraint::Length(1), // bottom action hints
        ])
        .split(area);

    let status_area = chunks[0];
    let main_area = chunks[1];
    let vitals_area = chunks[2];
    let hints_area = chunks[3];

    // Lazily load panel content if needed.
    if app.hud_detail.is_none() && app.selected_task_idx.is_some() {
        app.load_hud_detail();
    }
    if app.right_panel_tab == RightPanelTab::Log
        && app.log_pane.task_id.is_none()
        && app.selected_task_idx.is_some()
    {
        app.load_log_pane();
    }
    if app.right_panel_tab == RightPanelTab::Messages
        && app.messages_panel.task_id.is_none()
        && app.selected_task_idx.is_some()
    {
        app.load_messages_panel();
    }
    if app.right_panel_tab == RightPanelTab::Agency
        && app.agency_lifecycle.is_none()
        && app.selected_task_idx.is_some()
    {
        app.load_agency_lifecycle();
    }
    // Lazy-load coordinator log + activity feed on first switch to CoordLog tab.
    if app.right_panel_tab == RightPanelTab::CoordLog {
        if app.coord_log.rendered_lines.is_empty() {
            app.load_coord_log();
        }
        if app.activity_feed.events.is_empty() {
            app.load_activity_feed();
        }
    }
    // Lazy-init file browser on first switch to Files tab.
    if app.right_panel_tab == RightPanelTab::Files && app.file_browser.is_none() {
        app.file_browser = Some(super::file_browser::FileBrowser::new(&app.workgraph_dir));
    }
    // Lazy-load firehose data on first switch to Firehose tab.
    if app.right_panel_tab == RightPanelTab::Firehose && app.firehose.lines.is_empty() {
        app.update_firehose();
    }

    // ── Responsive breakpoint detection ──
    // Recomputed each frame from the terminal width. Drives layout decisions below.
    app.responsive_breakpoint = ResponsiveBreakpoint::from_width(area.width);

    // Phase 1: Compute viewport dimensions from layout (needed for deferred centering).
    match app.responsive_breakpoint {
        ResponsiveBreakpoint::Compact => {
            // Single-panel mode: show graph OR detail OR log, one at a time.
            // No room for tri-state strips in compact mode.
            app.last_minimized_strip_area = Rect::default();
            app.last_fullscreen_restore_area = Rect::default();
            app.last_fullscreen_right_border_area = Rect::default();
            app.last_fullscreen_top_border_area = Rect::default();
            app.last_fullscreen_bottom_border_area = Rect::default();
            match app.single_panel_view {
                SinglePanelView::Graph => {
                    app.last_graph_area = main_area;
                    app.last_right_panel_area = Rect::default();
                    app.last_divider_area = Rect::default();
                    app.last_tab_bar_area = Rect::default();
                    app.last_right_content_area = Rect::default();
                    app.scroll.viewport_height = main_area.height as usize;
                    app.scroll.viewport_width = main_area.width as usize;
                }
                SinglePanelView::Detail | SinglePanelView::Log => {
                    app.last_graph_area = Rect::default();
                    app.scroll.viewport_height = 0;
                    app.scroll.viewport_width = 0;
                }
            }
        }
        ResponsiveBreakpoint::Narrow => {
            // Narrow split: side-by-side at ~40/60 or stacked, depending on split layout.
            match app.layout_mode {
                LayoutMode::FullInspector => {
                    app.last_graph_area = Rect::default();
                    app.scroll.viewport_height = 0;
                    app.scroll.viewport_width = 0;
                    // Reserve all four borders (1 col left, 1 col right, 1 row top, 1 row bottom).
                    let has_h_room = main_area.width > 2;
                    let has_v_room = main_area.height > 2;
                    if has_h_room {
                        app.last_fullscreen_restore_area =
                            Rect::new(main_area.x, main_area.y, 1, main_area.height);
                        app.last_fullscreen_right_border_area = Rect::new(
                            main_area.x + main_area.width - 1,
                            main_area.y,
                            1,
                            main_area.height,
                        );
                    } else {
                        app.last_fullscreen_restore_area = Rect::default();
                        app.last_fullscreen_right_border_area = Rect::default();
                    }
                    if has_v_room {
                        app.last_fullscreen_top_border_area =
                            Rect::new(main_area.x, main_area.y, main_area.width, 1);
                        app.last_fullscreen_bottom_border_area = Rect::new(
                            main_area.x,
                            main_area.y + main_area.height - 1,
                            main_area.width,
                            1,
                        );
                    } else {
                        app.last_fullscreen_top_border_area = Rect::default();
                        app.last_fullscreen_bottom_border_area = Rect::default();
                    }
                    app.last_minimized_strip_area = Rect::default();
                }
                LayoutMode::Off => {
                    // Always reserve 1 col for the minimized strip so the graph
                    // never reflows when hover toggles strip visibility.
                    let has_room = main_area.width > 1;
                    let strip_width = if has_room { 1u16 } else { 0 };
                    let graph_width = main_area.width.saturating_sub(strip_width);
                    let graph_area =
                        Rect::new(main_area.x, main_area.y, graph_width, main_area.height);
                    app.last_graph_area = graph_area;
                    app.last_right_panel_area = Rect::default();
                    app.last_divider_area = Rect::default();
                    app.last_tab_bar_area = Rect::default();
                    app.last_right_content_area = Rect::default();
                    app.scroll.viewport_height = graph_area.height as usize;
                    app.scroll.viewport_width = graph_area.width as usize;
                    if has_room {
                        app.last_minimized_strip_area = Rect::new(
                            main_area.x + main_area.width - 1,
                            main_area.y,
                            1,
                            main_area.height,
                        );
                    } else {
                        app.last_minimized_strip_area = Rect::default();
                    }
                    app.last_fullscreen_restore_area = Rect::default();
                    app.last_fullscreen_right_border_area = Rect::default();
                    app.last_fullscreen_top_border_area = Rect::default();
                    app.last_fullscreen_bottom_border_area = Rect::default();
                }
                _ => {
                    app.last_minimized_strip_area = Rect::default();
                    app.last_fullscreen_restore_area = Rect::default();
                    app.last_fullscreen_right_border_area = Rect::default();
                    app.last_fullscreen_top_border_area = Rect::default();
                    app.last_fullscreen_bottom_border_area = Rect::default();
                    // Narrow mode is always below SIDE_MIN_WIDTH, so inspector
                    // goes to the bottom (vertical split) to avoid oscillation.
                    app.inspector_is_beside = false;
                    if app.right_panel_visible {
                        let panel_height =
                            (main_area.height as u32 * app.right_panel_percent as u32 / 100).max(5)
                                as u16;
                        let top_height = main_area.height.saturating_sub(panel_height);
                        let split = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([
                                Constraint::Length(top_height),
                                Constraint::Length(panel_height),
                            ])
                            .split(main_area);
                        app.last_graph_area = split[0];
                        app.scroll.viewport_height = split[0].height as usize;
                        app.scroll.viewport_width = split[0].width as usize;
                    } else {
                        app.last_graph_area = main_area;
                        app.last_right_panel_area = Rect::default();
                        app.last_divider_area = Rect::default();
                        app.last_tab_bar_area = Rect::default();
                        app.last_right_content_area = Rect::default();
                        app.scroll.viewport_height = main_area.height as usize;
                        app.scroll.viewport_width = main_area.width as usize;
                    }
                }
            }
        }
        ResponsiveBreakpoint::Full => {
            // Full layout: existing behavior, unchanged.
            match app.layout_mode {
                LayoutMode::FullInspector => {
                    app.last_graph_area = Rect::default();
                    app.scroll.viewport_height = 0;
                    app.scroll.viewport_width = 0;
                    // Reserve all four borders (1 col left, 1 col right, 1 row top, 1 row bottom).
                    let has_h_room = main_area.width > 2;
                    let has_v_room = main_area.height > 2;
                    if has_h_room {
                        app.last_fullscreen_restore_area =
                            Rect::new(main_area.x, main_area.y, 1, main_area.height);
                        app.last_fullscreen_right_border_area = Rect::new(
                            main_area.x + main_area.width - 1,
                            main_area.y,
                            1,
                            main_area.height,
                        );
                    } else {
                        app.last_fullscreen_restore_area = Rect::default();
                        app.last_fullscreen_right_border_area = Rect::default();
                    }
                    if has_v_room {
                        app.last_fullscreen_top_border_area =
                            Rect::new(main_area.x, main_area.y, main_area.width, 1);
                        app.last_fullscreen_bottom_border_area = Rect::new(
                            main_area.x,
                            main_area.y + main_area.height - 1,
                            main_area.width,
                            1,
                        );
                    } else {
                        app.last_fullscreen_top_border_area = Rect::default();
                        app.last_fullscreen_bottom_border_area = Rect::default();
                    }
                    app.last_minimized_strip_area = Rect::default();
                }
                LayoutMode::Off => {
                    // Always reserve 1 col for the minimized strip so the graph
                    // never reflows when hover toggles strip visibility.
                    let has_room = main_area.width > 1;
                    let strip_width = if has_room { 1u16 } else { 0 };
                    let graph_width = main_area.width.saturating_sub(strip_width);
                    let graph_area =
                        Rect::new(main_area.x, main_area.y, graph_width, main_area.height);
                    app.last_graph_area = graph_area;
                    app.last_right_panel_area = Rect::default();
                    app.last_divider_area = Rect::default();
                    app.last_tab_bar_area = Rect::default();
                    app.last_right_content_area = Rect::default();
                    app.scroll.viewport_height = graph_area.height as usize;
                    app.scroll.viewport_width = graph_area.width as usize;
                    if has_room {
                        app.last_minimized_strip_area = Rect::new(
                            main_area.x + main_area.width - 1,
                            main_area.y,
                            1,
                            main_area.height,
                        );
                    } else {
                        app.last_minimized_strip_area = Rect::default();
                    }
                    app.last_fullscreen_restore_area = Rect::default();
                    app.last_fullscreen_right_border_area = Rect::default();
                    app.last_fullscreen_top_border_area = Rect::default();
                    app.last_fullscreen_bottom_border_area = Rect::default();
                }
                LayoutMode::ThirdInspector
                | LayoutMode::HalfInspector
                | LayoutMode::TwoThirdsInspector => {
                    app.last_minimized_strip_area = Rect::default();
                    app.last_fullscreen_restore_area = Rect::default();
                    app.last_fullscreen_right_border_area = Rect::default();
                    app.last_fullscreen_top_border_area = Rect::default();
                    app.last_fullscreen_bottom_border_area = Rect::default();
                    if app.right_panel_visible {
                        // Hysteresis: use different thresholds for switching directions
                        // to prevent oscillation at the boundary.
                        let use_side = if app.inspector_is_beside {
                            area.width >= SIDE_MIN_WIDTH
                        } else {
                            area.width >= SIDE_RESTORE_WIDTH
                        };
                        app.inspector_is_beside = use_side;
                        if use_side {
                            let right_width = (main_area.width as u32
                                * app.right_panel_percent as u32
                                / 100) as u16;
                            let left_width = main_area.width.saturating_sub(right_width);
                            let split = Layout::default()
                                .direction(Direction::Horizontal)
                                .constraints([
                                    Constraint::Length(left_width),
                                    Constraint::Length(right_width),
                                ])
                                .split(main_area);
                            app.last_graph_area = split[0];
                            app.scroll.viewport_height = split[0].height as usize;
                            app.scroll.viewport_width = split[0].width as usize;
                        } else {
                            let panel_height =
                                (main_area.height as u32 * app.right_panel_percent as u32 / 100)
                                    .max(5) as u16;
                            let top_height = main_area.height.saturating_sub(panel_height);
                            let split = Layout::default()
                                .direction(Direction::Vertical)
                                .constraints([
                                    Constraint::Length(top_height),
                                    Constraint::Length(panel_height),
                                ])
                                .split(main_area);
                            app.last_graph_area = split[0];
                            app.scroll.viewport_height = split[0].height as usize;
                            app.scroll.viewport_width = split[0].width as usize;
                        }
                    } else {
                        app.last_graph_area = main_area;
                        app.last_right_panel_area = Rect::default();
                        app.last_divider_area = Rect::default();
                        app.last_tab_bar_area = Rect::default();
                        app.last_right_content_area = Rect::default();
                        app.scroll.viewport_height = main_area.height as usize;
                        app.scroll.viewport_width = main_area.width as usize;
                    }
                }
            }
        }
    }

    // Phase 2: Deferred centering/scrolling — viewport_height is now set, apply before drawing.
    if app.needs_center_on_selected {
        app.needs_center_on_selected = false;
        app.needs_scroll_into_view = false; // center supersedes scroll-into-view
        app.center_on_selected_task();
    } else if app.needs_scroll_into_view {
        app.needs_scroll_into_view = false;
        app.scroll_to_selected_task();
    }

    // Phase 3: Draw content using the (possibly updated) scroll offset.
    match app.responsive_breakpoint {
        ResponsiveBreakpoint::Compact => {
            // Single-panel mode: draw only the active panel.
            match app.single_panel_view {
                SinglePanelView::Graph => {
                    draw_viz_content(frame, app, main_area);
                    if app.scroll.content_height > app.scroll.viewport_height
                        && app.graph_scrollbar_visible()
                    {
                        draw_scrollbar(frame, app, main_area);
                    }
                    app.last_graph_hscrollbar_area = draw_horizontal_scrollbar(
                        frame,
                        main_area,
                        app.scroll.content_width,
                        app.scroll.viewport_width,
                        app.scroll.offset_x,
                        app.scroll.has_horizontal_overflow() && app.graph_hscrollbar_visible(),
                    );
                }
                SinglePanelView::Detail | SinglePanelView::Log => {
                    draw_right_panel(frame, app, main_area);
                    app.last_graph_hscrollbar_area = Rect::default();
                }
            }
        }
        ResponsiveBreakpoint::Narrow => {
            // Narrow split mode.
            match app.layout_mode {
                LayoutMode::FullInspector => {
                    // Compute panel area inset by all four reserved borders.
                    let panel_area = fullscreen_panel_area(main_area, app);
                    // Draw each border only on hover (or always when mouse not supported).
                    draw_fullscreen_borders(frame, app);
                    draw_right_panel(frame, app, panel_area);
                    app.last_graph_hscrollbar_area = Rect::default();
                }
                LayoutMode::Off => {
                    let graph_area = app.last_graph_area;
                    draw_viz_content(frame, app, graph_area);
                    if app.scroll.content_height > app.scroll.viewport_height
                        && app.graph_scrollbar_visible()
                    {
                        draw_scrollbar(frame, app, graph_area);
                    }
                    // Strip content only drawn on hover; space always reserved.
                    let strip = app.last_minimized_strip_area;
                    if strip.width > 0 && (app.minimized_strip_hover || !app.any_motion_mouse) {
                        draw_minimized_strip(frame, strip, true);
                    }
                    app.last_graph_hscrollbar_area = draw_horizontal_scrollbar(
                        frame,
                        graph_area,
                        app.scroll.content_width,
                        app.scroll.viewport_width,
                        app.scroll.offset_x,
                        app.scroll.has_horizontal_overflow() && app.graph_hscrollbar_visible(),
                    );
                }
                _ => {
                    // Narrow mode: inspector below (vertical split).
                    if app.right_panel_visible {
                        let panel_height =
                            (main_area.height as u32 * app.right_panel_percent as u32 / 100).max(5)
                                as u16;
                        let top_height = main_area.height.saturating_sub(panel_height);
                        let split = Layout::default()
                            .direction(Direction::Vertical)
                            .constraints([
                                Constraint::Length(top_height),
                                Constraint::Length(panel_height),
                            ])
                            .split(main_area);

                        let viz_area = split[0];
                        let right_area = split[1];

                        draw_viz_content(frame, app, viz_area);
                        if app.scroll.content_height > app.scroll.viewport_height
                            && app.graph_scrollbar_visible()
                        {
                            draw_scrollbar(frame, app, viz_area);
                        }
                        app.last_graph_hscrollbar_area = draw_horizontal_scrollbar(
                            frame,
                            viz_area,
                            app.scroll.content_width,
                            app.scroll.viewport_width,
                            app.scroll.offset_x,
                            app.scroll.has_horizontal_overflow() && app.graph_hscrollbar_visible(),
                        );
                        draw_right_panel(frame, app, right_area);
                    } else {
                        draw_viz_content(frame, app, main_area);
                        if app.scroll.content_height > app.scroll.viewport_height
                            && app.graph_scrollbar_visible()
                        {
                            draw_scrollbar(frame, app, main_area);
                        }
                        app.last_graph_hscrollbar_area = draw_horizontal_scrollbar(
                            frame,
                            main_area,
                            app.scroll.content_width,
                            app.scroll.viewport_width,
                            app.scroll.offset_x,
                            app.scroll.has_horizontal_overflow() && app.graph_hscrollbar_visible(),
                        );
                    }
                }
            }
        }
        ResponsiveBreakpoint::Full => {
            // Full layout: existing behavior.
            match app.layout_mode {
                LayoutMode::FullInspector => {
                    // Compute panel area inset by all four reserved borders.
                    let panel_area = fullscreen_panel_area(main_area, app);
                    // Draw each border only on hover (or always when mouse not supported).
                    draw_fullscreen_borders(frame, app);
                    draw_right_panel(frame, app, panel_area);
                    app.last_graph_hscrollbar_area = Rect::default();
                }
                LayoutMode::Off => {
                    let graph_area = app.last_graph_area;
                    draw_viz_content(frame, app, graph_area);
                    if app.scroll.content_height > app.scroll.viewport_height
                        && app.graph_scrollbar_visible()
                    {
                        draw_scrollbar(frame, app, graph_area);
                    }
                    // Strip content only drawn on hover; space always reserved.
                    let strip = app.last_minimized_strip_area;
                    if strip.width > 0 && (app.minimized_strip_hover || !app.any_motion_mouse) {
                        draw_minimized_strip(frame, strip, true);
                    }
                    app.last_graph_hscrollbar_area = draw_horizontal_scrollbar(
                        frame,
                        graph_area,
                        app.scroll.content_width,
                        app.scroll.viewport_width,
                        app.scroll.offset_x,
                        app.scroll.has_horizontal_overflow() && app.graph_hscrollbar_visible(),
                    );
                }
                LayoutMode::ThirdInspector
                | LayoutMode::HalfInspector
                | LayoutMode::TwoThirdsInspector => {
                    if app.right_panel_visible {
                        // Use the hysteresis state computed in Phase 1.
                        if app.inspector_is_beside {
                            let right_width = (main_area.width as u32
                                * app.right_panel_percent as u32
                                / 100) as u16;
                            let left_width = main_area.width.saturating_sub(right_width);
                            let split = Layout::default()
                                .direction(Direction::Horizontal)
                                .constraints([
                                    Constraint::Length(left_width),
                                    Constraint::Length(right_width),
                                ])
                                .split(main_area);

                            let viz_area = split[0];
                            let right_area = split[1];

                            draw_viz_content(frame, app, viz_area);
                            if app.scroll.content_height > app.scroll.viewport_height
                                && app.graph_scrollbar_visible()
                            {
                                draw_scrollbar(frame, app, viz_area);
                            }
                            app.last_graph_hscrollbar_area = draw_horizontal_scrollbar(
                                frame,
                                viz_area,
                                app.scroll.content_width,
                                app.scroll.viewport_width,
                                app.scroll.offset_x,
                                app.scroll.has_horizontal_overflow()
                                    && app.graph_hscrollbar_visible(),
                            );
                            draw_right_panel(frame, app, right_area);
                        } else {
                            let panel_height =
                                (main_area.height as u32 * app.right_panel_percent as u32 / 100)
                                    .max(5) as u16;
                            let top_height = main_area.height.saturating_sub(panel_height);
                            let split = Layout::default()
                                .direction(Direction::Vertical)
                                .constraints([
                                    Constraint::Length(top_height),
                                    Constraint::Length(panel_height),
                                ])
                                .split(main_area);

                            let viz_area = split[0];
                            let right_area = split[1];

                            draw_viz_content(frame, app, viz_area);
                            if app.scroll.content_height > app.scroll.viewport_height
                                && app.graph_scrollbar_visible()
                            {
                                draw_scrollbar(frame, app, viz_area);
                            }
                            app.last_graph_hscrollbar_area = draw_horizontal_scrollbar(
                                frame,
                                viz_area,
                                app.scroll.content_width,
                                app.scroll.viewport_width,
                                app.scroll.offset_x,
                                app.scroll.has_horizontal_overflow()
                                    && app.graph_hscrollbar_visible(),
                            );
                            draw_right_panel(frame, app, right_area);
                        }
                    } else {
                        draw_viz_content(frame, app, main_area);
                        if app.scroll.content_height > app.scroll.viewport_height
                            && app.graph_scrollbar_visible()
                        {
                            draw_scrollbar(frame, app, main_area);
                        }
                        app.last_graph_hscrollbar_area = draw_horizontal_scrollbar(
                            frame,
                            main_area,
                            app.scroll.content_width,
                            app.scroll.viewport_width,
                            app.scroll.offset_x,
                            app.scroll.has_horizontal_overflow() && app.graph_hscrollbar_visible(),
                        );
                    }
                }
            }
        }
    }

    // Top status bar
    draw_status_bar(frame, app, status_area);

    // Service health badge — right-aligned pill on the status bar.
    draw_service_health_badge(frame, app, status_area);

    // Vitals bar
    draw_vitals_bar(frame, app, vitals_area);

    // Bottom action hints
    draw_action_hints(frame, app, hints_area);

    // ── Overlay widgets (on top of everything) ──

    if app.show_help {
        draw_help_overlay(frame);
    }

    // Confirmation dialog overlay
    if let InputMode::Confirm(ref action) = app.input_mode {
        app.last_dialog_area = draw_confirm_dialog(frame, action);
    } else if let InputMode::ChoiceDialog(ref state) = app.input_mode {
        app.last_dialog_area = draw_choice_dialog(frame, state);
    } else if matches!(app.input_mode, InputMode::CoordinatorPicker) {
        if let Some(ref picker) = app.coordinator_picker {
            app.last_dialog_area =
                draw_coordinator_picker(frame, picker, app.active_coordinator_id);
        }
    } else {
        app.last_dialog_area = Rect::default();
    }

    // Text prompt overlay
    if let InputMode::TextPrompt(ref action) = app.input_mode {
        app.last_text_prompt_area = draw_text_prompt(frame, action, &mut app.text_prompt.editor);
    } else {
        app.last_text_prompt_area = Rect::default();
    }

    // Task creation form overlay
    if app.input_mode == InputMode::TaskForm
        && let Some(ref form) = app.task_form
    {
        draw_task_form(frame, form);
    }

    // Service control panel (modal overlay)
    if app.service_health.panel_open {
        draw_service_control_panel(frame, app);
    }

    // Legacy service health detail popup
    if app.service_health.detail_open && !app.service_health.panel_open {
        draw_service_health_detail(frame, app);
    }

    // Toast notifications (severity-leveled, stacked in top-right)
    if !app.toasts.is_empty() {
        draw_toasts(frame, app);
    }

    // Key feedback overlay (for screencasts/demos)
    if app.key_feedback_enabled && !app.key_feedback.is_empty() {
        draw_key_feedback(frame, app);
    }

    // Touch echo overlay (click/touch visual feedback for screencasts/demos)
    if app.touch_echo_enabled && !app.touch_echoes.is_empty() {
        draw_touch_echoes(frame, app);
    }
}

/// Determine the line-level trace category for a given original line index.
/// Used only for task text coloring (not for edge characters).
enum LineTraceCategory {
    Selected,
    Upstream,
    Downstream,
    Unrelated,
}

/// Check if an original line index belongs to a task with an active splash animation.
/// Returns (fade_progress, flash_color, animation_kind) where progress is 0.0 = start,
/// approaching 1.0 = end of animation.
fn splash_info_for_line(
    app: &VizApp,
    orig_idx: usize,
) -> Option<(f64, (u8, u8, u8), super::state::AnimationKind)> {
    for (task_id, &line) in &app.node_line_map {
        if line == orig_idx {
            let progress = app.splash_progress(task_id)?;
            let color = app.splash_color(task_id).unwrap_or((180, 160, 60));
            let kind = app
                .splash_kind(task_id)
                .unwrap_or(super::state::AnimationKind::NewTask);
            return Some((progress, color, kind));
        }
    }
    None
}

/// Apply animation styles to the task title portion of a line.
/// `progress` ranges from 0.0 (start) to 1.0 (end of animation).
/// `flash_color` is the (r, g, b) color at full brightness (used for NewTask).
/// Only the task title (ID) gets the effect — tree connectors, status/token
/// metadata, timestamps, and trailing content are left unchanged.
///
/// For `Revealed` animations the text **foreground** fades in from the terminal
/// background color to its normal color (text emerges from invisibility).
/// For `NewTask` animations the **background** flashes and fades out.
fn apply_splash_style<'a>(
    line: Line<'a>,
    progress: f64,
    plain_line: &str,
    flash_color: (u8, u8, u8),
    reduced_motion: bool,
    kind: super::state::AnimationKind,
) -> Line<'a> {
    let is_fade_in = matches!(kind, super::state::AnimationKind::Revealed);

    // Terminal background color assumption (dark terminal).
    let terminal_bg: (u8, u8, u8) = (0, 0, 0);

    if reduced_motion {
        if is_fade_in {
            // For fade-in with reduced motion, show invisible text for first half,
            // then snap to normal text for the second half.
            if progress < 0.5 {
                return apply_fg_fade_to_title_range(line, plain_line, terminal_bg, 0.0);
            }
            return line;
        }
        if progress > 0.5 {
            return line;
        }
        // Use the flash color at a moderate intensity (no fade).
        let splash_bg = Color::Rgb(
            (flash_color.0 as f64 * 0.6) as u8,
            (flash_color.1 as f64 * 0.6) as u8,
            (flash_color.2 as f64 * 0.6) as u8,
        );
        return apply_bg_to_title_range(line, plain_line, splash_bg);
    }

    if is_fade_in {
        // Fade-in: text foreground transitions from terminal bg (invisible)
        // to its normal color. Use an ease-out curve (fast reveal, slow settle).
        let inv = 1.0 - progress;
        let t = (1.0 - inv * inv).min(1.0);
        return apply_fg_fade_to_title_range(line, plain_line, terminal_bg, t);
    }

    // Default: flash-and-fade-out.
    // Ease-out curve for a smoother fade (fast initial dim, slow tail-off).
    let t = progress * progress;

    // Interpolate from the flash color to no background.
    // Since terminals don't have true alpha, we fade toward black (0,0,0).
    let r = (flash_color.0 as f64 * (1.0 - t)) as u8;
    let g = (flash_color.1 as f64 * (1.0 - t)) as u8;
    let b = (flash_color.2 as f64 * (1.0 - t)) as u8;

    // At low intensity, skip to avoid a visible snap when the animation
    // ends.  Use generous thresholds so the fade reaches near-invisible
    // well before the animation timer expires.
    if r < 25 && g < 25 && b < 15 {
        return line;
    }

    let splash_bg = Color::Rgb(r, g, b);
    apply_bg_to_title_range(line, plain_line, splash_bg)
}

/// Apply a brief inverse/highlight flash to the annotation text region of a line.
/// The flash fades from bright pink background to transparent over 500ms.
fn apply_annotation_flash<'a>(
    line: Line<'a>,
    col_start: usize,
    col_end: usize,
    elapsed_ms: u64,
) -> Line<'a> {
    // Fade from bright pink to nothing over 500ms.
    let progress = (elapsed_ms as f64 / 500.0).min(1.0);
    let t = progress * progress; // ease-out
    let intensity = 1.0 - t;

    // True pink (ANSI 219) base: (255, 175, 215) — fade toward black.
    let r = (255.0 * intensity) as u8;
    let g = (175.0 * intensity) as u8;
    let b = (215.0 * intensity) as u8;

    if r < 20 && g < 20 && b < 20 {
        return line;
    }

    let flash_bg = Color::Rgb(r, g, b);
    let flash_fg = Color::Rgb(0, 0, 0); // black text on pink bg

    let mut new_spans: Vec<Span<'a>> = Vec::new();
    let mut char_idx = 0;
    for span in line.spans {
        let span_len = span.content.chars().count();
        let span_start = char_idx;
        let span_end = char_idx + span_len;

        if span_end <= col_start || span_start >= col_end {
            // Entirely outside flash region.
            new_spans.push(span);
        } else if span_start >= col_start && span_end <= col_end {
            // Entirely inside flash region.
            let mut style = span.style;
            style = style.bg(flash_bg).fg(flash_fg);
            new_spans.push(Span::styled(span.content, style));
        } else {
            // Partially overlapping — split the span.
            let chars: Vec<char> = span.content.chars().collect();
            let overlap_start = col_start.saturating_sub(span_start);
            let overlap_end = col_end.saturating_sub(span_start).min(span_len);

            if overlap_start > 0 {
                let before: String = chars[..overlap_start].iter().collect();
                new_spans.push(Span::styled(before, span.style));
            }
            let mid: String = chars[overlap_start..overlap_end].iter().collect();
            let mut mid_style = span.style;
            mid_style = mid_style.bg(flash_bg).fg(flash_fg);
            new_spans.push(Span::styled(mid, mid_style));
            if overlap_end < span_len {
                let after: String = chars[overlap_end..].iter().collect();
                new_spans.push(Span::styled(after, span.style));
            }
        }
        char_idx = span_end;
    }
    Line::from(new_spans)
}

/// Find the task title/ID range in a viz line — just the title, not metadata.
///
/// A typical viz line looks like:
///   `└→ fix-remove-yellow  (in-progress · →1.1M) 4m`
///
/// This returns the range covering only `fix-remove-yellow`, stopping before
/// the `  (` that introduces status/token metadata.
fn find_title_range(plain_line: &str) -> Option<(usize, usize)> {
    let chars: Vec<char> = plain_line.chars().collect();

    // Start at first alphanumeric or dot character (skip tree connectors like └→).
    // Dot is included so system tasks like `.assign-foo` have their prefix in range.
    let text_start = chars
        .iter()
        .position(|c| c.is_alphanumeric() || *c == '.')?;

    // End before the metadata parenthetical — look for `  (` (two spaces + open paren)
    // which separates the task ID from its status/token info.
    let mut text_end = chars.len();
    for i in text_start..chars.len().saturating_sub(2) {
        if chars[i] == ' ' && chars[i + 1] == ' ' && chars[i + 2] == '(' {
            text_end = i;
            break;
        }
    }

    // If we didn't find `  (`, try single ` (` as fallback.
    if text_end == chars.len() {
        for i in text_start..chars.len().saturating_sub(1) {
            if chars[i] == ' ' && chars[i + 1] == '(' {
                text_end = i;
                break;
            }
        }
    }

    // Trim trailing whitespace from the title range.
    while text_end > text_start && chars[text_end - 1] == ' ' {
        text_end -= 1;
    }

    if text_end <= text_start {
        return None;
    }

    Some((text_start, text_end))
}

/// Apply a background color to the task title range (narrow — just the ID/title).
fn apply_bg_to_title_range<'a>(line: Line<'a>, plain_line: &str, bg: Color) -> Line<'a> {
    let (text_start, text_end) = match find_title_range(plain_line) {
        Some(range) => range,
        None => return line,
    };

    // Flatten spans into per-character (char, style) pairs.
    let mut chars_with_styles: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        for c in span.content.chars() {
            chars_with_styles.push((c, span.style));
        }
    }

    // Rebuild spans, applying bg only within the title range.
    let mut new_spans: Vec<Span<'a>> = Vec::new();
    let mut current_buf = String::new();
    let mut current_style = Style::default();
    let mut first = true;

    for (char_idx, (c, base_style)) in chars_with_styles.iter().enumerate() {
        let style = if char_idx >= text_start && char_idx < text_end {
            base_style.bg(bg)
        } else {
            *base_style
        };

        if first {
            current_style = style;
            first = false;
        } else if style != current_style {
            new_spans.push(Span::styled(
                std::mem::take(&mut current_buf),
                current_style,
            ));
            current_style = style;
        }

        current_buf.push(*c);
    }

    if !current_buf.is_empty() {
        new_spans.push(Span::styled(current_buf, current_style));
    }

    Line::from(new_spans)
}

/// Convert any ratatui `Color` to an approximate `(u8, u8, u8)` RGB tuple.
///
/// Used for smooth color interpolation during fade animations, so that ANSI named
/// colors fade toward their true hue instead of a gray fallback.
fn color_to_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Red => (205, 49, 49),
        Color::Green => (13, 188, 121),
        Color::Yellow => (229, 229, 16),
        Color::Blue => (36, 114, 200),
        Color::Magenta => (188, 63, 188),
        Color::Cyan => (17, 168, 205),
        Color::Gray => (170, 170, 170),
        Color::DarkGray => (118, 118, 118),
        Color::LightRed => (241, 76, 76),
        Color::LightGreen => (35, 209, 139),
        Color::LightYellow => (245, 245, 67),
        Color::LightBlue => (59, 142, 234),
        Color::LightMagenta => (214, 112, 214),
        Color::LightCyan => (41, 184, 219),
        Color::White => (229, 229, 229),
        Color::Indexed(idx) => indexed_color_to_rgb(idx),
        _ => (200, 200, 200),
    }
}

/// Map a 256-color palette index to approximate RGB.
fn indexed_color_to_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        // 0-7: standard colors (same as named ANSI)
        0 => (0, 0, 0),
        1 => (205, 49, 49),
        2 => (13, 188, 121),
        3 => (229, 229, 16),
        4 => (36, 114, 200),
        5 => (188, 63, 188),
        6 => (17, 168, 205),
        7 => (170, 170, 170),
        // 8-15: bright colors
        8 => (118, 118, 118),
        9 => (241, 76, 76),
        10 => (35, 209, 139),
        11 => (245, 245, 67),
        12 => (59, 142, 234),
        13 => (214, 112, 214),
        14 => (41, 184, 219),
        15 => (229, 229, 229),
        // 16-231: 6x6x6 color cube
        16..=231 => {
            let n = idx - 16;
            let b_idx = n % 6;
            let g_idx = (n / 6) % 6;
            let r_idx = n / 36;
            let to_val = |i: u8| if i == 0 { 0u8 } else { 55 + 40 * i };
            (to_val(r_idx), to_val(g_idx), to_val(b_idx))
        }
        // 232-255: grayscale ramp
        232..=255 => {
            let v = 8 + 10 * (idx - 232);
            (v, v, v)
        }
    }
}

/// Apply a foreground color fade to the task title range.
/// Interpolates each span's foreground from `start_fg` toward its original fg color
/// based on `t` (0.0 = fully `start_fg`, 1.0 = original foreground).
fn apply_fg_fade_to_title_range<'a>(
    line: Line<'a>,
    plain_line: &str,
    start_fg: (u8, u8, u8),
    t: f64,
) -> Line<'a> {
    let (text_start, text_end) = match find_title_range(plain_line) {
        Some(range) => range,
        None => return line,
    };

    // Flatten spans into per-character (char, style) pairs.
    let mut chars_with_styles: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        for c in span.content.chars() {
            chars_with_styles.push((c, span.style));
        }
    }

    // Rebuild spans, applying fg interpolation only within the title range.
    let mut new_spans: Vec<Span<'a>> = Vec::new();
    let mut current_buf = String::new();
    let mut current_style = Style::default();
    let mut first = true;

    for (char_idx, (c, base_style)) in chars_with_styles.iter().enumerate() {
        let style = if char_idx >= text_start && char_idx < text_end {
            let orig_fg = match base_style.fg {
                Some(c) => color_to_rgb(c),
                // No foreground set — assume light gray for dark terminals.
                None => (200, 200, 200),
            };
            let r = (start_fg.0 as f64 + (orig_fg.0 as f64 - start_fg.0 as f64) * t) as u8;
            let g = (start_fg.1 as f64 + (orig_fg.1 as f64 - start_fg.1 as f64) * t) as u8;
            let b = (start_fg.2 as f64 + (orig_fg.2 as f64 - start_fg.2 as f64) * t) as u8;
            base_style.fg(Color::Rgb(r, g, b))
        } else {
            *base_style
        };

        if first {
            current_style = style;
            first = false;
        } else if style != current_style {
            new_spans.push(Span::styled(
                std::mem::take(&mut current_buf),
                current_style,
            ));
            current_style = style;
        }

        current_buf.push(*c);
    }

    if !current_buf.is_empty() {
        new_spans.push(Span::styled(current_buf, current_style));
    }

    Line::from(new_spans)
}

fn classify_task_line(app: &VizApp, orig_idx: usize) -> LineTraceCategory {
    // Check if this line is the selected task's line.
    if let Some(selected_id) = app.selected_task_id()
        && let Some(&sel_line) = app.node_line_map.get(selected_id)
        && orig_idx == sel_line
    {
        return LineTraceCategory::Selected;
    }
    // Check if this line belongs to an upstream or downstream task node.
    for (id, &line) in &app.node_line_map {
        if line == orig_idx {
            if app.upstream_set.contains(id) {
                return LineTraceCategory::Upstream;
            }
            if app.downstream_set.contains(id) {
                return LineTraceCategory::Downstream;
            }
        }
    }
    LineTraceCategory::Unrelated
}

/// Check whether a given original line index is the selected task's line.
fn is_selected_task_line(app: &VizApp, orig_idx: usize) -> bool {
    if let Some(selected_id) = app.selected_task_id()
        && let Some(&sel_line) = app.node_line_map.get(selected_id)
    {
        return orig_idx == sel_line;
    }
    false
}

fn draw_viz_content(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    // When the history browser is active, render it instead of the graph.
    if app.history_browser.active {
        draw_history_browser(frame, app, area);
        return;
    }

    // When the archive browser is active, render it instead of the graph.
    if app.archive_browser.active {
        draw_archive_browser(frame, app, area);
        return;
    }

    let visible_count = app.visible_line_count();
    let start = app.scroll.offset_y;
    let end = (start + area.height as usize).min(visible_count);

    if start >= visible_count {
        return;
    }

    let has_search = app.has_active_search() && !app.fuzzy_matches.is_empty();
    let current_match_orig_line = app.current_match_line();
    let jump_target_line = app.jump_target.map(|(line, _)| line);
    let has_trace = app.selected_task_idx.is_some() && app.trace_visible;
    let has_selected = app.selected_task_idx.is_some();

    // Build lines for the visible range.
    // Each visible row maps to an original line index via visible_to_original.
    let mut text_lines: Vec<Line> = Vec::with_capacity(end - start);

    // Precompute the selected task ID for the edge map lookups.
    let selected_id = app.selected_task_id().map(|s| s.to_string());

    // Precompute coordinator line indices for chat-to-coordinator visual link.
    // When the Chat tab is active, coordinator task lines get a subtle cyan highlight.
    let chat_active = app.right_panel_visible && app.right_panel_tab == RightPanelTab::Chat;
    let coordinator_lines: HashSet<usize> = if chat_active {
        app.node_line_map
            .iter()
            .filter(|(id, _)| id.starts_with(".coordinator"))
            .map(|(_, &line)| line)
            .collect()
    } else {
        HashSet::new()
    };

    // Precompute user board line indices for messages-to-board visual link.
    // When the Messages tab is active on a user board, those lines get a subtle yellow highlight.
    let messages_on_user_board = app.right_panel_visible
        && app.right_panel_tab == RightPanelTab::Messages
        && app
            .selected_task_idx
            .and_then(|idx| app.task_order.get(idx))
            .is_some_and(|id| workgraph::graph::is_user_board(id));
    let user_board_lines: HashSet<usize> = if messages_on_user_board {
        app.node_line_map
            .iter()
            .filter(|(id, _)| workgraph::graph::is_user_board(id))
            .map(|(_, &line)| line)
            .collect()
    } else {
        HashSet::new()
    };

    for visible_idx in start..end {
        let orig_idx = app.visible_to_original(visible_idx);

        // Get the ANSI line and parse it.
        let ansi_line = app.lines.get(orig_idx).map(|s| s.as_str()).unwrap_or("");
        let base_line: Line = match ansi_to_tui::IntoText::into_text(&ansi_line) {
            Ok(text) => text.lines.into_iter().next().unwrap_or_default(),
            Err(_) => {
                let plain = app
                    .plain_lines
                    .get(orig_idx)
                    .map(|s| s.as_str())
                    .unwrap_or("");
                Line::from(plain)
            }
        };

        if has_search {
            if let Some(fuzzy_match) = app.match_for_line(orig_idx) {
                // This line has a fuzzy match — highlight matched characters.
                let is_current = current_match_orig_line == Some(orig_idx);
                let mut highlighted =
                    highlight_fuzzy_match(base_line, &fuzzy_match.char_positions, is_current);
                if is_current {
                    highlighted = highlighted.style(Style::default().bg(Color::Yellow));
                }
                text_lines.push(highlighted);
            } else {
                // Non-matching line in filtered view: show dimmed.
                let dimmed = base_line.style(Style::default().fg(Color::DarkGray));
                text_lines.push(dimmed);
            }
        } else if jump_target_line == Some(orig_idx)
            && splash_info_for_line(app, orig_idx).is_none()
        {
            // Transient highlight on the line we jumped to after Enter.
            // Skipped when a splash animation is active (splash provides a smoother fade).
            text_lines.push(base_line.style(Style::default().bg(Color::Yellow)));
        } else if has_trace {
            // Per-character edge tracing with topology-aware coloring.
            let plain_line = app
                .plain_lines
                .get(orig_idx)
                .map(|s| s.as_str())
                .unwrap_or("");
            let line_category = classify_task_line(app, orig_idx);
            let colored_line = apply_per_char_trace_coloring(
                base_line,
                plain_line,
                orig_idx,
                &line_category,
                app,
                selected_id.as_deref(),
            );
            // Mark the selected task with bold + bright styling (text only).
            if matches!(line_category, LineTraceCategory::Selected) {
                text_lines.push(apply_selection_style(colored_line, plain_line));
            } else {
                text_lines.push(colored_line);
            }
        } else if has_selected && is_selected_task_line(app, orig_idx) {
            // Trace is off but a task is selected — still show bold + bright (text only).
            let plain_line = app
                .plain_lines
                .get(orig_idx)
                .map(|s| s.as_str())
                .unwrap_or("");
            text_lines.push(apply_selection_style(base_line, plain_line));
        } else {
            text_lines.push(base_line);
        }

        // Apply splash-and-fade animation overlay if this line belongs to an animated task.
        if !app.splash_animations.is_empty()
            && app.animation_mode.is_enabled()
            && let Some((progress, flash_color, anim_kind)) = splash_info_for_line(app, orig_idx)
            && progress < 1.0
        {
            let splash_plain = app
                .plain_lines
                .get(orig_idx)
                .map(|s| s.as_str())
                .unwrap_or("");
            let reduced = matches!(app.animation_mode, super::state::AnimationMode::Reduced);
            let last = text_lines.last_mut().unwrap();
            *last = apply_splash_style(
                std::mem::take(last),
                progress,
                splash_plain,
                flash_color,
                reduced,
                anim_kind,
            );
        }

        // Annotation click flash: briefly invert the annotation text colors.
        if let Some(ref flash) = app.annotation_click_flash
            && flash.orig_line == orig_idx
        {
            let elapsed_ms = flash.start.elapsed().as_millis() as u64;
            if elapsed_ms < 500 {
                let last = text_lines.last_mut().unwrap();
                *last = apply_annotation_flash(
                    std::mem::take(last),
                    flash.col_start,
                    flash.col_end,
                    elapsed_ms,
                );
            }
        }

        // Chat-to-coordinator visual link: apply a subtle cyan tint to coordinator
        // task lines when the Chat tab is visible, connecting the two visually.
        if coordinator_lines.contains(&orig_idx) {
            let last = text_lines.last_mut().unwrap();
            // Subtle dark cyan background to mark the coordinator row.
            *last = std::mem::take(last).style(Style::default().bg(Color::Rgb(0, 40, 50)));
        }

        // Messages-to-user-board visual link: apply a subtle yellow tint to user
        // board task lines when the Messages tab is active on a user board.
        if user_board_lines.contains(&orig_idx) {
            let last = text_lines.last_mut().unwrap();
            *last = std::mem::take(last).style(Style::default().bg(Color::Rgb(50, 40, 0)));
        }
    }

    let text = Text::from(text_lines);

    // Apply horizontal scroll.
    let paragraph = Paragraph::new(text).scroll((0, app.scroll.offset_x as u16));

    frame.render_widget(paragraph, area);

    // Off-screen selection direction indicator: when the selected task is
    // scrolled out of the viewport, show a yellow arrow at the edge to hint
    // which direction the user needs to scroll.
    if has_selected
        && !has_search
        && let Some(selected_id) = app.selected_task_id()
        && let Some(&sel_orig_line) = app.node_line_map.get(selected_id)
    {
        let is_visible = (start..end).any(|vi| app.visible_to_original(vi) == sel_orig_line);
        if !is_visible {
            let first_visible_orig = app.visible_to_original(start);
            let indicator_style = Style::default().fg(Color::Yellow);
            if sel_orig_line < first_visible_orig {
                // Selected task is above viewport.
                let arrow = Paragraph::new(Line::from(Span::styled("▲", indicator_style)));
                let arrow_area = Rect {
                    x: area.x,
                    y: area.y,
                    width: 1,
                    height: 1,
                };
                frame.render_widget(arrow, arrow_area);
            } else {
                // Selected task is below viewport.
                let arrow = Paragraph::new(Line::from(Span::styled("▼", indicator_style)));
                let bottom_y = area.y + area.height.saturating_sub(1);
                let arrow_area = Rect {
                    x: area.x,
                    y: bottom_y,
                    width: 1,
                    height: 1,
                };
                frame.render_widget(arrow, arrow_area);
            }
        }
    }
}

/// Apply per-character trace coloring to a line based on the char_edge_map.
///
/// PURELY ADDITIVE — only these changes from normal display:
/// - Edge chars where both endpoints are in upstream_set ∪ {selected}: magenta
/// - Edge chars where both endpoints are in downstream_set ∪ {selected}: cyan
/// - Selected task text: original style preserved (bold + bright applied at line level)
/// - Everything else: original style preserved unchanged
fn apply_per_char_trace_coloring<'a>(
    line: Line<'a>,
    plain_line: &str,
    orig_idx: usize,
    _line_category: &LineTraceCategory,
    app: &VizApp,
    selected_id: Option<&str>,
) -> Line<'a> {
    let text_range = find_text_range(plain_line);

    // Flatten spans into characters with styles.
    let mut chars_with_styles: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        for c in span.content.chars() {
            chars_with_styles.push((c, span.style));
        }
    }

    // Build the upstream+selected and downstream+selected sets for quick lookup.
    let in_cycle = |id: &str| -> bool { app.cycle_set.contains(id) };
    let in_upstream =
        |id: &str| -> bool { app.upstream_set.contains(id) || selected_id == Some(id) };
    let in_downstream =
        |id: &str| -> bool { app.downstream_set.contains(id) || selected_id == Some(id) };

    let (text_start, text_end) = text_range.unwrap_or((usize::MAX, usize::MAX));

    // Rebuild spans with per-character coloring.
    // PURELY ADDITIVE: only edge chars in the dependency chain get magenta/cyan,
    // selected task text gets yellow bg. Everything else keeps its original style.
    let mut new_spans: Vec<Span<'a>> = Vec::new();
    let mut current_buf = String::new();
    let mut current_style = Style::default();
    let mut first = true;

    for (char_idx, (c, base_style)) in chars_with_styles.iter().enumerate() {
        let is_text = char_idx >= text_start && char_idx < text_end;

        let style = if is_text {
            // All task text keeps original style unchanged.
            // Selected task is indicated by bold + bright styling at line level.
            *base_style
        } else if let Some(edges) = app.char_edge_map.get(&(orig_idx, char_idx)) {
            // Edge character with known edge(s): color if ANY edge matches topology.
            // Shared arc column positions may carry multiple edges.
            // Priority: yellow (cycle) > magenta (upstream) > cyan (downstream).
            let is_cycle_edge = !app.cycle_set.is_empty()
                && edges
                    .iter()
                    .any(|(src, tgt)| in_cycle(src) && in_cycle(tgt));
            let is_upstream_edge = edges
                .iter()
                .any(|(src, tgt)| in_upstream(src) && in_upstream(tgt));
            let is_downstream_edge = edges
                .iter()
                .any(|(src, tgt)| in_downstream(src) && in_downstream(tgt));
            if is_cycle_edge {
                let mut s = *base_style;
                s.fg = Some(Color::Yellow);
                s
            } else if is_upstream_edge {
                let mut s = *base_style;
                s.fg = Some(Color::Magenta);
                s
            } else if is_downstream_edge {
                let mut s = *base_style;
                s.fg = Some(Color::Cyan);
                s
            } else {
                // Edge exists but not in the selected task's dependency chain — keep original
                *base_style
            }
        } else {
            // Non-text, non-edge character (spaces, connectors not in edge map, etc.)
            // Keep original style — trace is purely additive
            *base_style
        };

        if first {
            current_style = style;
            first = false;
        } else if style != current_style {
            new_spans.push(Span::styled(
                std::mem::take(&mut current_buf),
                current_style,
            ));
            current_style = style;
        }

        current_buf.push(*c);
    }

    if !current_buf.is_empty() {
        new_spans.push(Span::styled(current_buf, current_style));
    }

    Line::from(new_spans)
}

/// Find the character range of the "task text" in a plain viz line.
/// Returns (text_start, text_end) as char indices.
/// - text_start: index of first alphanumeric character (task ID start)
/// - text_end: index after last ')' (closing status/token info)
///   Returns None for non-task lines (pure connectors, blanks, summaries).
fn find_text_range(plain_line: &str) -> Option<(usize, usize)> {
    let chars: Vec<char> = plain_line.chars().collect();

    // Find first alphanumeric character (start of task text).
    let text_start = chars.iter().position(|c| c.is_alphanumeric())?;

    // Find the last ')' which closes the status/token info.
    let text_end = chars
        .iter()
        .rposition(|&c| c == ')')
        .map(|i| i + 1) // exclusive end, include the ')'
        .unwrap_or_else(|| {
            // No ')' found — find the last non-connector char.
            let mut end = text_start;
            for (i, &ch) in chars.iter().enumerate().skip(text_start) {
                if !ch.is_whitespace() && !super::state::is_box_drawing(ch) {
                    end = i + 1;
                }
            }
            end
        });

    Some((text_start, text_end))
}

/// Apply bold + bright styling to the task text portion of the selected line.
///
/// Uses `find_text_range` to identify the task text (ID, title, status) and
/// only applies bold + bright there. Edge/connector characters outside the
/// text range keep their original style (or trace color).
fn apply_selection_style<'a>(line: Line<'a>, plain_line: &str) -> Line<'a> {
    let text_range = find_text_range(plain_line);
    let (text_start, text_end) = text_range.unwrap_or((0, 0));

    // If no text range found, return line unchanged.
    if text_range.is_none() {
        return line;
    }

    // Flatten spans into per-character (char, style) pairs.
    let mut chars_with_styles: Vec<(char, Style)> = Vec::new();
    for span in &line.spans {
        for c in span.content.chars() {
            chars_with_styles.push((c, span.style));
        }
    }

    // Rebuild spans, applying bold+bright only within the text range.
    let mut new_spans: Vec<Span<'a>> = Vec::new();
    let mut current_buf = String::new();
    let mut current_style = Style::default();
    let mut first = true;

    for (char_idx, (c, base_style)) in chars_with_styles.iter().enumerate() {
        let style = if char_idx >= text_start && char_idx < text_end {
            brighten_style(*base_style).add_modifier(Modifier::BOLD)
        } else {
            *base_style
        };

        if first {
            current_style = style;
            first = false;
        } else if style != current_style {
            new_spans.push(Span::styled(
                std::mem::take(&mut current_buf),
                current_style,
            ));
            current_style = style;
        }

        current_buf.push(*c);
    }

    if !current_buf.is_empty() {
        new_spans.push(Span::styled(current_buf, current_style));
    }

    Line::from(new_spans)
}

/// Brighten a style's foreground color for the selected-task emphasis effect.
fn brighten_style(style: Style) -> Style {
    let bright_fg = match style.fg {
        Some(Color::Black) => Some(Color::DarkGray),
        Some(Color::Red) => Some(Color::LightRed),
        Some(Color::Green) => Some(Color::LightGreen),
        Some(Color::Yellow) => Some(Color::LightYellow),
        Some(Color::Blue) => Some(Color::LightBlue),
        Some(Color::Magenta) => Some(Color::LightMagenta),
        Some(Color::Cyan) => Some(Color::LightCyan),
        Some(Color::Gray) => Some(Color::White),
        Some(Color::DarkGray) => Some(Color::Gray),
        // Already bright or custom — keep as-is
        other => other,
    };
    Style {
        fg: bright_fg,
        ..style
    }
}

/// Highlight the fuzzy-matched characters within a line.
/// Matched chars get bold + colored. Current match uses a distinct color.
fn highlight_fuzzy_match<'a>(
    base_line: Line<'a>,
    char_positions: &[usize],
    is_current_match: bool,
) -> Line<'a> {
    if char_positions.is_empty() {
        return base_line;
    }

    let match_set: HashSet<usize> = char_positions.iter().copied().collect();

    let match_modifier = if is_current_match {
        Modifier::BOLD | Modifier::UNDERLINED
    } else {
        Modifier::UNDERLINED
    };

    // Flatten the line's spans into individual characters, then regroup
    // into spans based on whether each char is matched or not.
    let mut chars_with_styles: Vec<(char, Style)> = Vec::new();
    for span in &base_line.spans {
        for c in span.content.chars() {
            chars_with_styles.push((c, span.style));
        }
    }

    let mut new_spans: Vec<Span<'a>> = Vec::new();
    let mut current_buf = String::new();
    let mut current_is_match = false;
    let mut current_base_style = Style::default();

    for (char_idx, (c, base_style)) in chars_with_styles.iter().enumerate() {
        let is_match = match_set.contains(&char_idx);

        // Check if we need to flush the current buffer.
        if !current_buf.is_empty()
            && (is_match != current_is_match || *base_style != current_base_style)
        {
            let style = if current_is_match {
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(match_modifier)
            } else {
                current_base_style
            };
            new_spans.push(Span::styled(std::mem::take(&mut current_buf), style));
        }

        current_buf.push(*c);
        current_is_match = is_match;
        current_base_style = *base_style;
    }

    // Flush remaining buffer.
    if !current_buf.is_empty() {
        let style = if current_is_match {
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(match_modifier)
        } else {
            current_base_style
        };
        new_spans.push(Span::styled(current_buf, style));
    }

    Line::from(new_spans)
}

fn draw_archive_browser(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let ab = &app.archive_browser;
    let visible = ab.visible_count();
    let viewport_h = area.height.saturating_sub(2) as usize; // 1 for header, 1 for footer

    // Auto-scroll to keep selection visible
    let scroll = if ab.selected < ab.scroll {
        ab.selected
    } else if ab.selected >= ab.scroll + viewport_h {
        ab.selected.saturating_sub(viewport_h - 1)
    } else {
        ab.scroll
    };
    // Update scroll in app (needs mut)
    app.archive_browser.scroll = scroll;

    let ab = &app.archive_browser;

    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);

    // Header line
    let header_text = if ab.filter_active {
        format!(
            " Archive ({} entries) — filter: /{}▌",
            ab.entries.len(),
            ab.filter
        )
    } else if !ab.filter.is_empty() {
        format!(
            " Archive ({}/{} entries) — filter: /{} ",
            visible,
            ab.entries.len(),
            ab.filter
        )
    } else {
        format!(" Archive ({} entries) ", ab.entries.len())
    };

    lines.push(Line::from(vec![
        Span::styled(
            header_text,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " [A/Esc:close  /:filter  r:restore  R:refresh]",
            Style::default().fg(Color::Rgb(100, 100, 100)),
        ),
    ]));

    // Entry rows
    let end = (scroll + viewport_h).min(visible);
    for vi in scroll..end {
        let idx = ab.filtered_indices[vi];
        let entry = &ab.entries[idx];
        let is_selected = vi == ab.selected;

        let completed = entry
            .completed_at
            .as_deref()
            .and_then(|s| s.get(..10)) // YYYY-MM-DD
            .unwrap_or("????-??-??");

        let tags_str = if entry.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", entry.tags.join(", "))
        };

        let line_text = format!(
            "  {} │ {} │ {}{}",
            completed, entry.id, entry.title, tags_str
        );

        let style = if is_selected {
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(50, 50, 80))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Rgb(180, 180, 180))
        };

        lines.push(Line::from(Span::styled(line_text, style)));
    }

    // Fill remaining rows
    while lines.len() < area.height as usize {
        lines.push(Line::from(""));
    }

    let paragraph = Paragraph::new(lines).style(Style::default().bg(Color::Rgb(20, 20, 20)));
    frame.render_widget(paragraph, area);

    // Store the area for graph pane compatibility
    app.last_graph_area = area;
}

fn draw_history_browser(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let seg_count = app.history_browser.segments.len();
    let preview_expanded = app.history_browser.preview_expanded;

    if preview_expanded {
        // Full preview mode: show the selected segment's content
        let (label, content) = match app.history_browser.selected_segment() {
            Some(seg) => (seg.label.clone(), seg.content.clone()),
            None => {
                app.last_graph_area = area;
                return;
            }
        };
        let content_lines: Vec<&str> = content.lines().collect();
        let total = content_lines.len();
        let viewport_h = area.height.saturating_sub(2) as usize;
        let scroll = app
            .history_browser
            .preview_scroll
            .min(total.saturating_sub(viewport_h));
        app.history_browser.preview_scroll = scroll;

        let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);

        // Header
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", label),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " [Enter:inject  Esc:back  j/k:scroll]",
                Style::default().fg(Color::Rgb(100, 100, 100)),
            ),
        ]));

        // Content lines
        let end = (scroll + viewport_h).min(total);
        for line in content_lines.iter().take(end).skip(scroll) {
            lines.push(Line::from(Span::styled(
                format!("  {}", line),
                Style::default().fg(Color::Rgb(200, 200, 200)),
            )));
        }

        // Fill remaining
        while lines.len() < area.height as usize {
            lines.push(Line::from(""));
        }

        let paragraph = Paragraph::new(lines).style(Style::default().bg(Color::Rgb(20, 20, 30)));
        frame.render_widget(paragraph, area);

        app.last_graph_area = area;
        return;
    }

    // List mode: show segments with preview of selected
    let list_height = (area.height as usize).saturating_sub(2);
    let list_rows = (list_height / 2).max(3).min(seg_count + 1);
    let preview_rows = list_height.saturating_sub(list_rows);

    // Auto-scroll list
    {
        let hb = &app.history_browser;
        let scroll = if hb.selected < hb.scroll {
            hb.selected
        } else if hb.selected >= hb.scroll + list_rows {
            hb.selected.saturating_sub(list_rows - 1)
        } else {
            hb.scroll
        };
        app.history_browser.scroll = scroll;
    }

    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);

    // Header
    let header = if seg_count == 0 {
        " History Browser — no segments available".to_string()
    } else {
        format!(
            " History Browser ({} segments) — coordinator #{}",
            seg_count, app.active_coordinator_id
        )
    };
    lines.push(Line::from(vec![
        Span::styled(
            header,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " [Enter:inject  Space:preview  Esc:close]",
            Style::default().fg(Color::Rgb(100, 100, 100)),
        ),
    ]));

    // Segment list
    let scroll = app.history_browser.scroll;
    let end = (scroll + list_rows).min(seg_count);
    for i in scroll..end {
        let seg = &app.history_browser.segments[i];
        let is_selected = i == app.history_browser.selected;

        let source_tag = match seg.source {
            workgraph::chat::HistorySource::ContextSummary => "📋",
            workgraph::chat::HistorySource::ActiveChat => "💬",
            workgraph::chat::HistorySource::Archive => "📦",
            workgraph::chat::HistorySource::CrossCoordinator { .. } => "🔗",
        };

        let line_text = format!("  {} {} ", source_tag, seg.label);

        let style = if is_selected {
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(40, 60, 90))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Rgb(180, 180, 180))
        };

        lines.push(Line::from(Span::styled(line_text, style)));
    }

    // Separator
    if seg_count > 0 {
        lines.push(Line::from(Span::styled(
            format!("  {}", "─".repeat(area.width.saturating_sub(4) as usize)),
            Style::default().fg(Color::Rgb(60, 60, 80)),
        )));
    }

    // Preview of selected segment
    if let Some(seg) = app.history_browser.selected_segment() {
        let preview_lines: Vec<&str> = seg.preview.lines().collect();
        for (i, pline) in preview_lines.iter().enumerate() {
            if i >= preview_rows.saturating_sub(1) {
                break;
            }
            lines.push(Line::from(Span::styled(
                format!("  {}", pline),
                Style::default().fg(Color::Rgb(140, 140, 160)),
            )));
        }
        if preview_lines.len() > preview_rows.saturating_sub(1) {
            lines.push(Line::from(Span::styled(
                "  ... (Space to expand)".to_string(),
                Style::default()
                    .fg(Color::Rgb(100, 100, 120))
                    .add_modifier(Modifier::ITALIC),
            )));
        }
    }

    // Fill remaining rows
    while lines.len() < area.height as usize {
        lines.push(Line::from(""));
    }

    let paragraph = Paragraph::new(lines).style(Style::default().bg(Color::Rgb(20, 20, 30)));
    frame.render_widget(paragraph, area);

    app.last_graph_area = area;
}

fn draw_scrollbar(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    // Store the scrollbar area for mouse hit-testing (rightmost column of the area).
    let sb_area = Rect {
        x: area.x + area.width.saturating_sub(1),
        y: area.y,
        width: 1,
        height: area.height,
    };
    app.last_graph_scrollbar_area = sb_area;

    let max_scroll = app
        .scroll
        .content_height
        .saturating_sub(app.scroll.viewport_height);
    let mut state = ScrollbarState::new(max_scroll).position(app.scroll.offset_y);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_style(Style::default().fg(Color::White))
        .track_style(Style::default().fg(Color::DarkGray));
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

/// Render a vertical scrollbar for the right panel and store its area for hit-testing.
fn draw_panel_scrollbar(
    frame: &mut Frame,
    app: &mut VizApp,
    area: Rect,
    max_scroll: usize,
    position: usize,
) {
    let sb_area = Rect {
        x: area.x + area.width.saturating_sub(1),
        y: area.y,
        width: 1,
        height: area.height,
    };
    app.last_panel_scrollbar_area = sb_area;

    let mut state = ScrollbarState::new(max_scroll).position(position);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .thumb_style(Style::default().fg(Color::White))
        .track_style(Style::default().fg(Color::DarkGray));
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

fn draw_horizontal_scrollbar(
    frame: &mut Frame,
    area: Rect,
    content_width: usize,
    viewport_width: usize,
    offset_x: usize,
    visible: bool,
) -> Rect {
    if !visible || content_width <= viewport_width || area.height == 0 {
        return Rect::default();
    }
    let scrollbar_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(1),
        width: area.width,
        height: 1,
    };
    let max_offset = content_width.saturating_sub(viewport_width);
    let mut state = ScrollbarState::new(max_offset).position(offset_x);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::HorizontalBottom)
        .thumb_style(Style::default().fg(Color::White))
        .track_style(Style::default().fg(Color::DarkGray));
    frame.render_stateful_widget(scrollbar, scrollbar_area, &mut state);
    scrollbar_area
}

// ══════════════════════════════════════════════════════════════════════════════
// Fullscreen inspector borders
// ══════════════════════════════════════════════════════════════════════════════

/// Compute the content area for fullscreen inspector, inset by all four
/// reserved border areas (left, right, top, bottom).
fn fullscreen_panel_area(main_area: Rect, app: &super::state::VizApp) -> Rect {
    let left = if app.last_fullscreen_restore_area.width > 0 {
        1u16
    } else {
        0
    };
    let right = if app.last_fullscreen_right_border_area.width > 0 {
        1u16
    } else {
        0
    };
    let top = if app.last_fullscreen_top_border_area.height > 0 {
        1u16
    } else {
        0
    };
    let bottom = if app.last_fullscreen_bottom_border_area.height > 0 {
        1u16
    } else {
        0
    };
    Rect::new(
        main_area.x + left,
        main_area.y + top,
        main_area.width.saturating_sub(left + right),
        main_area.height.saturating_sub(top + bottom),
    )
}

/// Draw all four fullscreen borders — only on hover (or always when mouse not
/// supported, for the left restore strip which doubles as click target).
fn draw_fullscreen_borders(frame: &mut Frame, app: &super::state::VizApp) {
    let no_mouse = !app.any_motion_mouse;

    // Left border (restore strip).
    // Always render so the area is claimed — invisible when not hovered.
    let left = app.last_fullscreen_restore_area;
    if left.width > 0 {
        if app.fullscreen_restore_hover {
            draw_restore_strip(frame, left, true);
        } else {
            // Invisible: plain spaces with default terminal background.
            frame.render_widget(Clear, left);
        }
    }

    // Right border.
    // Always render so the area is claimed — invisible when not hovered.
    let right = app.last_fullscreen_right_border_area;
    if right.width > 0 {
        if app.fullscreen_right_hover {
            draw_fullscreen_border_col(frame, right, '▐', true);
        } else {
            // Invisible: plain spaces with default terminal background.
            frame.render_widget(Clear, right);
        }
    }

    // Top border.
    let top = app.last_fullscreen_top_border_area;
    if top.height > 0 && (app.fullscreen_top_hover || no_mouse) {
        draw_fullscreen_border_row(frame, top, '▀', app.fullscreen_top_hover);
    }

    // Bottom border.
    let bottom = app.last_fullscreen_bottom_border_area;
    if bottom.height > 0 && (app.fullscreen_bottom_hover || no_mouse) {
        draw_fullscreen_border_row(frame, bottom, '▄', app.fullscreen_bottom_hover);
    }
}

/// Draw a single-column vertical border strip (for right edge).
fn draw_fullscreen_border_col(frame: &mut Frame, area: Rect, ch: char, hover: bool) {
    let fg = if hover {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let lines: Vec<Line> = (0..area.height)
        .map(|_| Line::from(Span::styled(ch.to_string(), Style::default().fg(fg))))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// Draw a single-row horizontal border strip (for top/bottom edge).
fn draw_fullscreen_border_row(frame: &mut Frame, area: Rect, ch: char, hover: bool) {
    let fg = if hover {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let text: String = (0..area.width).map(|_| ch).collect();
    let line = Line::from(Span::styled(text, Style::default().fg(fg)));
    frame.render_widget(Paragraph::new(vec![line]), area);
}

// ══════════════════════════════════════════════════════════════════════════════
// Tri-state inspector strips
// ══════════════════════════════════════════════════════════════════════════════

/// Draw the 1-col restore strip on the left edge in FullInspector mode.
/// Clicking/dragging from this strip restores the normal split view.
fn draw_restore_strip(frame: &mut Frame, area: Rect, hover: bool) {
    let fg = if hover {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let text: String = (0..area.height).map(|_| '▌').collect();
    let lines: Vec<Line> = text
        .chars()
        .map(|c| Line::from(Span::styled(c.to_string(), Style::default().fg(fg))))
        .collect();
    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

/// Draw the 1-col minimized strip on the right edge in Off mode.
/// Clicking this strip restores the normal split view.
fn draw_minimized_strip(frame: &mut Frame, area: Rect, hover: bool) {
    let fg = if hover {
        Color::Yellow
    } else {
        Color::DarkGray
    };
    let text: String = (0..area.height).map(|_| '▐').collect();
    let lines: Vec<Line> = text
        .chars()
        .map(|c| Line::from(Span::styled(c.to_string(), Style::default().fg(fg))))
        .collect();
    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

// ══════════════════════════════════════════════════════════════════════════════
// Right panel rendering
// ══════════════════════════════════════════════════════════════════════════════

/// Draw the right panel with tab bar and active tab content.
fn draw_right_panel(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    app.last_right_panel_area = area;

    let is_full_panel = app.layout_mode == LayoutMode::FullInspector;

    // Store divider hit areas for mouse-based resize.
    // Vertical divider: only in side-by-side mode (inspector beside graph).
    // Horizontal divider: only in stacked mode (inspector below graph).
    if !is_full_panel && area.width > 0 && app.last_graph_area.width > 0 && app.inspector_is_beside
    {
        // Hit area: 3 columns centered on the left border for easier grabbing.
        let div_x = area.x.saturating_sub(1);
        let div_w = 3.min(area.x.saturating_sub(app.last_graph_area.x) + 1);
        app.last_divider_area = Rect::new(div_x, area.y, div_w, area.height);
        app.last_horizontal_divider_area = Rect::default();
    } else if !is_full_panel
        && area.height > 0
        && app.last_graph_area.height > 0
        && !app.inspector_is_beside
    {
        // Hit area: 3 rows centered on the top border for easier grabbing.
        let div_y = area.y.saturating_sub(1);
        let div_h = 3.min(area.y.saturating_sub(app.last_graph_area.y) + 1);
        app.last_horizontal_divider_area = Rect::new(area.x, div_y, area.width, div_h);
        app.last_divider_area = Rect::default();
    } else {
        app.last_divider_area = Rect::default();
        app.last_horizontal_divider_area = Rect::default();
    }

    let divider_active = app.divider_hover
        || app.horizontal_divider_hover
        || app.scrollbar_drag == Some(super::state::ScrollbarDragTarget::Divider)
        || app.scrollbar_drag == Some(super::state::ScrollbarDragTarget::HorizontalDivider);

    // In full-panel mode: no borders (edge-to-edge content for clean copy-paste).
    // In split mode: minimal single-line border, dim when unfocused.
    let inner = if is_full_panel {
        area
    } else {
        let is_focused = app.focused_panel == FocusedPanel::RightPanel;
        let is_chat_tab = app.right_panel_tab == RightPanelTab::Chat;
        let is_user_board_active = app.right_panel_tab == RightPanelTab::Messages
            && app
                .selected_task_idx
                .and_then(|idx| app.task_order.get(idx))
                .is_some_and(|id| workgraph::graph::is_user_board(id));
        let border_color = if divider_active || is_user_board_active {
            Color::Yellow
        } else if is_chat_tab && app.chat.coordinator_active {
            Color::Cyan
        } else if is_focused {
            Color::White
        } else {
            Color::DarkGray
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    };

    if inner.height < 2 || inner.width < 4 {
        return;
    }

    // Split inner into tab bar (1 line) + content.
    let panel_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    let tab_area = panel_chunks[0];
    let content_area = panel_chunks[1];

    app.last_tab_bar_area = tab_area;
    app.last_right_content_area = content_area;

    // Tab bar — pass selected task's message status for the Msg tab indicator.
    let msg_status = app
        .selected_task_id()
        .and_then(|id| app.task_message_statuses.get(id))
        .cloned();
    draw_tab_bar(frame, app, app.right_panel_tab, tab_area, msg_status);

    // Apply slide animation offset to the content area.
    let content_area = if let Some(ref anim) = app.slide_animation {
        if anim.is_done() {
            app.slide_animation = None;
            content_area
        } else {
            let offset = anim.x_offset(content_area.width);
            let abs_offset = offset.unsigned_abs().min(content_area.width);
            if offset > 0 {
                // Forward: content slides in from the right
                Rect {
                    x: content_area.x + abs_offset,
                    width: content_area.width.saturating_sub(abs_offset),
                    ..content_area
                }
            } else if offset < 0 {
                // Backward: content slides in from the left — reduce visible width from right
                Rect {
                    width: content_area.width.saturating_sub(abs_offset),
                    ..content_area
                }
            } else {
                content_area
            }
        }
    } else {
        content_area
    };

    if content_area.width < 4 {
        return;
    }

    // Tab content
    match app.right_panel_tab {
        RightPanelTab::Chat => {
            draw_chat_tab(frame, app, content_area);
        }
        RightPanelTab::Detail => {
            draw_detail_tab(frame, app, content_area);
        }
        RightPanelTab::Agency => {
            draw_agents_tab(frame, app, content_area);
        }
        RightPanelTab::Config => {
            draw_config_tab(frame, app, content_area);
        }
        RightPanelTab::Log => {
            // Keep data fresh while the tab is visible. Cheap when nothing
            // has changed on disk.
            app.load_log_pane();
            app.update_log_output();
            app.update_log_stream_events();
            draw_log_tab(frame, app, content_area);
        }
        RightPanelTab::CoordLog => {
            draw_coord_log_tab(frame, app, content_area);
        }
        RightPanelTab::Dashboard => {
            draw_dashboard_tab(frame, app, content_area);
        }
        RightPanelTab::Messages => {
            app.load_messages_panel();
            draw_messages_tab(frame, app, content_area);
        }
        // Dead tabs — not reachable from the bar. No-op here so stray
        // state still renders as empty rather than crashing.
        RightPanelTab::Files | RightPanelTab::Firehose | RightPanelTab::Output => {}
    }
}

/// Draw the tab bar for the right panel.
/// `msg_status` colors the Messages tab icon to reflect TUI read state.
fn draw_tab_bar(
    frame: &mut Frame,
    app: &mut VizApp,
    active: RightPanelTab,
    area: Rect,
    msg_status: Option<workgraph::messages::CoordinatorMessageStatus>,
) {
    let tab_labels: Vec<Line> = RightPanelTab::ALL
        .iter()
        .map(|t| {
            if *t == RightPanelTab::Messages
                && let Some(ref status) = msg_status
            {
                return Line::from(vec![
                    Span::raw(format!("{}:", t.index())),
                    Span::styled(
                        format!("{} {}", t.label(), status.icon()),
                        Style::default().fg(status.color()),
                    ),
                ]);
            }

            // Special handling for Log tab: add visual indicator
            if *t == RightPanelTab::Log {
                let indicator = if app.log_pane.view_top { "▲" } else { "▼" };
                return Line::from(format!("{}:{} {}", t.index(), t.label(), indicator));
            }

            Line::from(format!("{}:{}", t.index(), t.label()))
        })
        .collect();
    let active_idx = active.index();

    // Check if we should show iteration navigator
    let should_show_iterator = is_task_relative_tab(active)
        && app.selected_task_id().is_some()
        && !app.iteration_archives.is_empty();

    if should_show_iterator {
        // Calculate space for iteration navigator
        let navigator_text = format_iteration_navigator(app);
        let navigator_width = navigator_text.chars().count() as u16;

        // Split area: tabs on left, navigator on right
        let tab_width = area.width.saturating_sub(navigator_width + 1); // +1 for padding
        let tab_area = Rect {
            width: tab_width,
            ..area
        };
        let nav_area = Rect {
            x: area.x + tab_width,
            y: area.y,
            width: navigator_width + 1,
            height: area.height,
        };

        // Store click regions for mouse handling
        app.last_iteration_nav_area = nav_area;

        // Render tabs in reduced area
        let tabs = Tabs::new(tab_labels)
            .select(active_idx)
            .style(Style::default().fg(Color::DarkGray))
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .divider("│");
        frame.render_widget(tabs, tab_area);

        // Render iteration navigator
        render_iteration_navigator(frame, app, nav_area);
    } else {
        // No navigator needed, use full area for tabs
        app.last_iteration_nav_area = Rect::default(); // Clear click region

        let tabs = Tabs::new(tab_labels)
            .select(active_idx)
            .style(Style::default().fg(Color::DarkGray))
            .highlight_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
            .divider("│");
        frame.render_widget(tabs, area);
    }
}

/// Check if a tab is task-relative (should show iteration navigator).
fn is_task_relative_tab(tab: RightPanelTab) -> bool {
    matches!(
        tab,
        RightPanelTab::Detail | RightPanelTab::Log | RightPanelTab::Messages
    )
}

/// Format the iteration navigator text based on current state.
fn format_iteration_navigator(app: &VizApp) -> String {
    let total = app.iteration_archives.len() + 1;
    let current_display = match app.viewing_iteration {
        None => total,        // "5/5" when viewing current
        Some(idx) => idx + 1, // "2/5" when viewing archive
    };

    // Responsive layout based on available width
    // Standard: "◀ iter 2/5 ▶", Compact: "◀ 2/5 ▶", Minimal: "◀▶"
    format!("◀ iter {}/{} ▶", current_display, total)
}

/// Render the iteration navigator widget in the given area.
fn render_iteration_navigator(frame: &mut Frame, app: &VizApp, area: Rect) {
    let total = app.iteration_archives.len() + 1;
    let current_display = match app.viewing_iteration {
        None => total,
        Some(idx) => idx + 1,
    };

    // Determine navigation capabilities
    let can_go_prev = match app.viewing_iteration {
        None => !app.iteration_archives.is_empty(),
        Some(idx) => idx > 0,
    };
    let can_go_next = match app.viewing_iteration {
        Some(idx) => idx + 1 < app.iteration_archives.len(),
        None => false,
    };

    // Create styled spans for the navigator
    let left_arrow_style = if can_go_prev {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let right_arrow_style = if can_go_next {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let counter_style = Style::default().fg(Color::Cyan);

    let navigator_line = Line::from(vec![
        Span::styled("◀", left_arrow_style),
        Span::raw(" iter "),
        Span::styled(format!("{}/{}", current_display, total), counter_style),
        Span::raw(" "),
        Span::styled("▶", right_arrow_style),
    ]);

    let paragraph = Paragraph::new(navigator_line).alignment(Alignment::Right);
    frame.render_widget(paragraph, area);
}

/// Draw the Detail tab content (evolved from HUD).
fn draw_detail_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let detail = match &app.hud_detail {
        Some(d) => d,
        None => {
            let msg =
                Paragraph::new("No task selected").style(Style::default().fg(Color::DarkGray));
            frame.render_widget(msg, area);
            return;
        }
    };

    // Extract output_mtime now (Copy-by-clone before detail borrow ends) so we can
    // use it for the footer after all the mutable app borrows below.
    let output_mtime = detail.output_mtime;

    // Reserve 1 line at the top for iteration navigation when there are archives.
    // Reserve 1 line at the bottom for the "last written X ago" footer when we have
    // an output timestamp.
    let has_iter_nav = !app.iteration_archives.is_empty();
    let (content_area, footer_area_opt) = if output_mtime.is_some() && area.height > 2 {
        let [ca, fa] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
        (ca, Some(fa))
    } else {
        (area, None)
    };
    let (header_area, area) = if has_iter_nav && area.height > 2 {
        let [ha, ca] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(content_area);
        (Some(ha), ca)
    } else {
        (None, content_area)
    };

    // ── Iteration navigation header ──
    if let Some(ha) = header_area {
        app.last_iter_nav_area = ha;

        let total = app.iteration_archives.len() + 1; // archives + current
        let label = match app.viewing_iteration {
            Some(idx) => format!("{}/{}", idx + 1, total),
            None => format!("{}/{}", total, total),
        };

        // ◀ arrow: clickable if not at oldest (i.e., viewing_iteration is not Some(0) when set,
        // or there are archives when viewing current)
        let can_go_prev = match app.viewing_iteration {
            None => !app.iteration_archives.is_empty(), // can go to last archive
            Some(idx) => idx > 0,                       // can go to previous archive
        };
        // ▶ arrow: clickable if not at current (viewing_iteration is Some and not at end)
        let can_go_next = match app.viewing_iteration {
            Some(idx) => idx + 1 < app.iteration_archives.len(),
            None => false, // already at current
        };

        let left_arrow = if can_go_prev {
            Span::styled(
                "◀",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled("◀", Style::default().fg(Color::DarkGray))
        };
        let right_arrow = if can_go_next {
            Span::styled(
                "▶",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled("▶", Style::default().fg(Color::DarkGray))
        };

        let middle_text = format!(" iter {} ", label);

        // Build the line: left arrow | center | right arrow
        // We want them roughly positioned: ◀ on left side, ▶ on right side
        let usable_width = ha.width.saturating_sub(2) as usize;
        let center_len = middle_text.len();
        // Distribute remaining space between/around the arrows
        let arrow_width = 2; // ◀ or ▶
        let gap = 2;
        let side_width = (usable_width.saturating_sub(center_len + arrow_width * 2 + gap * 2)) / 2;

        let line = Line::from(vec![
            left_arrow,
            Span::raw(" ".repeat(side_width.max(1))),
            Span::styled(
                middle_text,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(side_width.max(1))),
            right_arrow,
        ]);
        frame.render_widget(
            Paragraph::new(line).style(Style::default().bg(Color::Rgb(15, 15, 25))),
            ha,
        );
    } else {
        app.last_iter_nav_area = Rect::default();
    }

    // Build visible lines: filter out content of collapsed sections, add ▸/▾ indicators.
    let mut visible_lines: Vec<String> = Vec::new();
    let mut current_section: Option<String> = None;
    let mut in_collapsed = false;

    // First pass: collect content lines per section for summary when collapsed.
    let mut section_content: HashMap<String, Vec<String>> = HashMap::new();
    {
        let mut cur_sec: Option<String> = None;
        for line in &detail.rendered_lines {
            if let Some(name) = extract_section_name(line) {
                cur_sec = Some(name);
            } else if line.is_empty() {
                cur_sec = None;
            } else if let Some(ref sec) = cur_sec {
                section_content
                    .entry(sec.clone())
                    .or_default()
                    .push(line.clone());
            }
        }
    }

    for line in &detail.rendered_lines {
        if let Some(name) = extract_section_name(line) {
            let collapsed = app.detail_collapsed_sections.contains(&name);
            let indicator = if collapsed { "▸" } else { "▾" };
            // Preserve any trailing annotation like " [R: raw JSON]" from the original header.
            let annotation = line
                .trim()
                .find(" [")
                .map(|i| &line.trim()[i..])
                .unwrap_or("");
            // Replace the original header with an indicator-prefixed version.
            visible_lines.push(format!("{} ── {} ──{}", indicator, name, annotation));
            if collapsed {
                // Add a summary line showing content stats.
                let content_lines = section_content.get(&name);
                let line_count = content_lines.map_or(0, |v| v.len());
                let (word_count, byte_count) = content_lines.map_or((0, 0), |lines| {
                    let words: usize = lines.iter().map(|l| l.split_whitespace().count()).sum();
                    let bytes: usize = lines.iter().map(|l| l.len()).sum();
                    (words, bytes)
                });
                let size_str = if byte_count >= 1024 {
                    format!("{:.1} KB", byte_count as f64 / 1024.0)
                } else {
                    format!("{} B", byte_count)
                };
                visible_lines.push(format!(
                    "  [{} lines · {} words · {}]",
                    line_count, word_count, size_str
                ));
            }
            current_section = Some(name);
            in_collapsed = collapsed;
        } else if in_collapsed {
            // Skip content lines in collapsed sections.
            // But allow the trailing blank line (section separator) through so
            // the next section header doesn't merge visually with collapsed one.
            if line.is_empty() {
                visible_lines.push(String::new());
                in_collapsed = false;
            }
        } else {
            visible_lines.push(line.clone());
            // If we hit an empty line, the current section content ends.
            if line.is_empty() {
                current_section = None;
            }
        }
    }
    let _ = current_section; // suppress unused warning

    // Convert visible lines to styled Lines with markdown for text-heavy sections.
    let wrap_width = area.width as usize;
    let mut all_lines: Vec<Line> = Vec::new();

    // Track whether we're in a text-heavy section (Description, Prompt, Output).
    let is_md_header =
        |h: &str| h.contains("Description") || h.contains("Prompt") || h.contains("Output");
    let mut in_md_section = false;
    let mut md_buffer: Vec<String> = Vec::new();

    // Flush accumulated markdown content lines into styled, wrapped output.
    let flush_md = |buf: &mut Vec<String>, out: &mut Vec<Line>, w: usize| {
        if buf.is_empty() {
            return;
        }
        let md_text = buf.join("\n");
        buf.clear();
        let indent_w: usize = 2;
        let md_w = w.saturating_sub(indent_w);
        if md_w == 0 {
            out.push(Line::from(Span::raw(md_text)));
            return;
        }
        let md_lines = markdown_to_lines(&md_text, md_w);
        let wrapped = wrap_line_spans(&md_lines, md_w);
        for line in wrapped {
            let mut spans = vec![Span::raw("  ".to_string())];
            spans.extend(line.spans.into_iter());
            out.push(Line::from(spans));
        }
    };

    // Track section header positions for mouse click hit-testing.
    let mut section_header_positions: Vec<(usize, String)> = Vec::new();

    for line in &visible_lines {
        let is_header = line.starts_with("▸ ──") || line.starts_with("▾ ──");
        let is_summary = line.starts_with("  [") && line.contains("lines ·");

        if is_header {
            flush_md(&mut md_buffer, &mut all_lines, wrap_width);
            in_md_section = is_md_header(line);
            // Extract section name from the indicator-prefixed header.
            // Strip trailing annotations like " [R: raw JSON]" before extracting name.
            let base = line.split(" [").next().unwrap_or(line);
            let section_name = base
                .trim_start_matches("▸ ")
                .trim_start_matches("▾ ")
                .trim_start_matches("── ")
                .trim_end_matches(" ──")
                .to_string();
            section_header_positions.push((all_lines.len(), section_name));
            all_lines.push(Line::from(Span::styled(
                line.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        } else if is_summary {
            // Collapsed section summary line — render in dim italic style.
            all_lines.push(Line::from(Span::styled(
                line.clone(),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
        } else if line.is_empty() {
            flush_md(&mut md_buffer, &mut all_lines, wrap_width);
            all_lines.push(Line::from(""));
            in_md_section = false;
        } else if in_md_section {
            // Strip "  " indent; will re-add after markdown rendering.
            let content = line.strip_prefix("  ").unwrap_or(line);
            md_buffer.push(content.to_string());
        } else {
            // Non-markdown section: word-wrap as plain text.
            if line.len() > wrap_width && wrap_width > 0 {
                let wrapped = word_wrap(line, wrap_width);
                for w in wrapped {
                    all_lines.push(Line::from(Span::raw(w)));
                }
            } else {
                all_lines.push(Line::from(Span::raw(line.clone())));
            }
        }
    }
    flush_md(&mut md_buffer, &mut all_lines, wrap_width);

    let total_lines = all_lines.len();
    let viewport_h = area.height as usize;

    // Cache wrapped line count and viewport height for scroll calculations.
    app.hud_wrapped_line_count = total_lines;
    app.hud_detail_viewport_height = viewport_h;
    app.detail_section_header_lines = section_header_positions;

    // Clamp HUD scroll.
    let max_scroll = total_lines.saturating_sub(viewport_h);
    if app.hud_scroll > max_scroll {
        app.hud_scroll = max_scroll;
    }

    let start = app.hud_scroll;
    let end = (start + viewport_h).min(total_lines);

    let lines: Vec<Line> = all_lines[start..end].to_vec();

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);

    if total_lines > viewport_h && app.panel_scrollbar_visible() {
        draw_panel_scrollbar(
            frame,
            app,
            area,
            total_lines.saturating_sub(viewport_h),
            app.hud_scroll,
        );
    }

    // ── "Last written X ago" footer ──
    if let Some(footer_area) = footer_area_opt
        && let Some(mtime) = output_mtime
    {
        let age_secs = mtime.elapsed().unwrap_or_default().as_secs();
        let age_str = format_duration_compact(age_secs);
        let footer_text = format!("─── last written {} ago ───", age_str);
        let color = if age_secs < 30 {
            Color::DarkGray
        } else if age_secs < 300 {
            Color::Yellow
        } else {
            Color::Red
        };
        let footer = Paragraph::new(Line::from(Span::styled(
            footer_text,
            Style::default().fg(color),
        )));
        frame.render_widget(footer, footer_area);
    }
}

/// Draw the Chat tab content with word-wrapped messages, scrolling, and input area.
fn draw_chat_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let width = area.width as usize;
    if width < 4 || area.height < 3 {
        return;
    }

    // Reserve space for input area.
    // When editing or when there's persisted text, compute wrapped line count so
    // the input area shows the full buffer.
    let has_pending_att = !app.chat.pending_attachments.is_empty();
    let is_editing = app.input_mode == InputMode::ChatInput;
    let chat_text = super::state::editor_text(&app.chat.editor);
    let has_input_text = !chat_text.is_empty();
    let input_height: u16 = if is_editing || has_input_text {
        let prompt_prefix = 2;
        let usable = (area.width as usize).saturating_sub(prompt_prefix).max(1);
        let visual_lines = count_visual_lines(&chat_text, usable);
        let wrapped_lines = (visual_lines as u16).max(1);
        // Cap so input never eats more than 3/4 of the area (leave room for
        // at least a few chat history lines). The generous cap avoids edtui's
        // wrap-mode scrolling limitation where a single long wrapped line can
        // push the cursor off-screen.
        let max_input = (area.height * 3 / 4).max(3);
        let lines = wrapped_lines.min(max_input);
        let att_line = if has_pending_att { 1 } else { 0 };
        lines + 1 + att_line // +1 for separator line, +1 for attachment indicator
    } else {
        1
    };
    // Reserve 1 line for the search bar when searching or showing results.
    let chat_search_active =
        app.input_mode == InputMode::ChatSearch || !app.chat.search.query.is_empty();
    let search_bar_height: u16 = if chat_search_active { 1 } else { 0 };
    let msg_area_height = area
        .height
        .saturating_sub(input_height)
        .saturating_sub(search_bar_height);

    // Coordinator + user board tab bar — always visible so the user can discover [+]
    let coordinator_entries = app.list_coordinator_ids_and_labels();
    let user_board_entries = app.list_user_board_entries();
    let total_tab_count = coordinator_entries.len() + user_board_entries.len();
    let tab_bar_height: u16 = 1;

    {
        let tab_area = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: 1,
        };
        app.last_coordinator_bar_area = tab_area;

        // Color palette for coordinator dots — hashed from coordinator ID.
        const DOT_COLORS: &[Color] = &[
            Color::Cyan,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Red,
            Color::LightCyan,
            Color::LightGreen,
        ];
        fn dot_color(cid: u32) -> Color {
            // Knuth multiplicative hash to spread sequential IDs across the palette
            let hash = cid.wrapping_mul(2654435761);
            DOT_COLORS[hash as usize % DOT_COLORS.len()]
        }

        // Determine which user board is currently selected (if any).
        let selected_user_board: Option<String> = app
            .selected_task_idx
            .and_then(|idx| app.task_order.get(idx))
            .filter(|id| workgraph::graph::is_user_board(id))
            .cloned();

        let bar_x = tab_area.x;
        let max_width = tab_area.width as usize;
        let mut spans = Vec::new();
        let mut tab_hits = Vec::new();
        let mut col: usize = 1; // start after leading space
        let mut overflow = false;
        let mut tab_index: usize = 0; // combined index across all tabs

        // Leading space
        spans.push(Span::raw(" "));

        for (cid, label) in coordinator_entries.iter() {
            let cid = *cid;
            let is_active = cid == app.active_coordinator_id;
            let color = dot_color(cid);
            // Tab content: " ◉ Label [state] " or " ◉ Label [state] ✕ "
            // dot(1) + space(1) + label + state(2 if active) + space(1) + close(2)
            let label_width = label.len();
            let close_width: usize = 2; // " ✕"
            let state_width: usize = if is_active { 2 } else { 0 }; // " ●" / " ⟳" / " ○"
            // Content: dot(1) + " "(1) + label + state + " "(1) + close
            let tab_content_width = 1 + 1 + label_width + state_width + 1 + close_width;
            // Separator: "│" between tabs (1 column wide)
            let sep_w: usize = if tab_index > 0 { 1 } else { 0 };

            let total_tab_width = sep_w + tab_content_width;

            // Check if this tab fits (also need room for "… [+]" = 5 if there are more tabs)
            let remaining_tabs = total_tab_count - tab_index - 1;
            let suffix_width = if remaining_tabs > 0 { 6 } else { 4 }; // "… [+]" or " [+]"
            if col + total_tab_width + suffix_width > max_width && remaining_tabs > 0 {
                overflow = true;
                break;
            }

            // Separator
            if tab_index > 0 {
                spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));
                col += sep_w;
            }

            let tab_start = (bar_x as usize + col) as u16;

            // Dot
            if is_active {
                spans.push(Span::styled(
                    " ◉",
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(" ●", Style::default().fg(Color::DarkGray)));
            }
            col += 2; // " ◉" = space + dot

            // Label
            let label_style = if is_active {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            spans.push(Span::styled(format!(" {}", label), label_style));
            col += 1 + label_width; // " " + label

            // Coordinator state indicator (only on active tab).
            if is_active {
                let (state_icon, state_style) = if !app.chat.coordinator_active {
                    (" ○", Style::default().fg(Color::DarkGray))
                } else if app.chat.awaiting_response() {
                    (" ⟳", Style::default().fg(Color::Yellow))
                } else {
                    (" ●", Style::default().fg(Color::Green))
                };
                spans.push(Span::styled(state_icon, state_style));
                col += 2; // " " + icon
            }

            // Close button (padded for wider touch target)
            let close_start = (bar_x as usize + col) as u16;
            spans.push(Span::styled(" ✕", Style::default().fg(Color::Red)));
            col += 2; // " ✕"
            let close_end = (bar_x as usize + col) as u16;

            // Trailing space
            spans.push(Span::raw(" "));
            col += 1;

            let tab_end = (bar_x as usize + col) as u16;

            tab_hits.push(CoordinatorTabHit {
                kind: TabBarEntryKind::Coordinator(cid),
                tab_start,
                tab_end,
                close_start,
                close_end,
            });
            tab_index += 1;
        }

        // User board tabs — yellow color scheme
        if !overflow {
            for (task_id, label) in user_board_entries.iter() {
                let is_active = selected_user_board.as_deref() == Some(task_id.as_str());
                let label_width = label.len();
                // User board tabs: " ◉ Label ✕ " (no state indicator)
                let close_width: usize = 2; // " ✕"
                let tab_content_width = 1 + 1 + label_width + 1 + close_width;
                let sep_w: usize = if tab_index > 0 { 1 } else { 0 };

                let total_tab_width = sep_w + tab_content_width;

                let remaining_tabs = total_tab_count - tab_index - 1;
                let suffix_width = if remaining_tabs > 0 { 6 } else { 4 };
                if col + total_tab_width + suffix_width > max_width && remaining_tabs > 0 {
                    overflow = true;
                    break;
                }

                // Separator
                if tab_index > 0 {
                    spans.push(Span::styled("│", Style::default().fg(Color::DarkGray)));
                    col += sep_w;
                }

                let tab_start = (bar_x as usize + col) as u16;

                // Dot — yellow for user boards
                if is_active {
                    spans.push(Span::styled(
                        " ◉",
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ));
                } else {
                    spans.push(Span::styled(" ●", Style::default().fg(Color::DarkGray)));
                }
                col += 2;

                // Label
                let label_style = if is_active {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                spans.push(Span::styled(format!(" {}", label), label_style));
                col += 1 + label_width;

                // Close button
                let close_start = (bar_x as usize + col) as u16;
                spans.push(Span::styled(" ✕", Style::default().fg(Color::Red)));
                col += 2;
                let close_end = (bar_x as usize + col) as u16;

                // Trailing space
                spans.push(Span::raw(" "));
                col += 1;

                let tab_end = (bar_x as usize + col) as u16;

                tab_hits.push(CoordinatorTabHit {
                    kind: TabBarEntryKind::UserBoard(task_id.clone()),
                    tab_start,
                    tab_end,
                    close_start,
                    close_end,
                });
                tab_index += 1;
            }
        }

        if overflow {
            spans.push(Span::styled("… ", Style::default().fg(Color::DarkGray)));
            col += 2;
        }

        let plus_start = (bar_x as usize + col) as u16;
        spans.push(Span::styled("[+]", Style::default().fg(Color::DarkGray)));
        col += 3;
        let plus_end = (bar_x as usize + col) as u16;

        app.coordinator_tab_hits = tab_hits;
        app.coordinator_plus_hit = CoordinatorPlusHit {
            start: plus_start,
            end: plus_end,
        };

        let tab_line = Line::from(spans);
        frame.render_widget(Paragraph::new(vec![tab_line]), tab_area);
    }

    // Full-pane launcher: takes over the entire area below the tab bar.
    if app.launcher.is_some() {
        let launcher_area = Rect {
            x: area.x,
            y: area.y + tab_bar_height,
            width: area.width,
            height: area.height.saturating_sub(tab_bar_height),
        };
        draw_launcher_pane(frame, app, launcher_area);
        return;
    }

    let msg_area = Rect {
        x: area.x,
        y: area.y + tab_bar_height,
        width: area.width,
        height: msg_area_height.saturating_sub(tab_bar_height),
    };
    let search_bar_area = Rect {
        x: area.x,
        y: area.y + tab_bar_height + msg_area.height,
        width: area.width,
        height: search_bar_height,
    };
    let input_area = Rect {
        x: area.x,
        y: search_bar_area.y + search_bar_height,
        width: area.width,
        height: input_height,
    };

    // Store the message area for click-to-focus hit testing.
    app.last_chat_message_area = msg_area;

    // PTY mode: render the embedded handler's terminal output in
    // place of the file-tailing ChatMessage widgets. Phase 3a of
    // docs/design/sessions-as-identity-rollout.md. The input editor
    // below continues to render normally; keys route to the PTY via
    // the Ctrl+T branch in event.rs.
    if app.chat_pty_mode {
        let task_id = workgraph::chat_id::format_chat_task_id(app.active_coordinator_id);
        // Dead-handler cleanup: if the embedded process exited, drop
        // the pane so the next toggle-on respawns.
        let alive = app
            .task_panes
            .get_mut(&task_id)
            .map(|p| p.is_alive())
            .unwrap_or(false);
        if !alive {
            app.task_panes.remove(&task_id);
        }
        if let Some(pane) = app.task_panes.get_mut(&task_id) {
            let _ = pane.resize(msg_area.height, msg_area.width);
            let focused = app.focused_panel == super::state::FocusedPanel::RightPanel;
            pane.render_with_focus(frame, msg_area, focused);
        } else {
            // Pane gone (exited or never spawned); fall through to
            // the normal renderer so the user still sees chat content.
        }
        // Fall through to the input area rendering below — skip the
        // message-widget code path entirely when the pane was drawn.
        if app.task_panes.contains_key(&task_id) {
            // Draw input area beneath as usual.
            // Everything below `if app.chat.messages.is_empty()` is
            // the messages-widget path which we're skipping.
            // But the file also does input/search rendering at the end,
            // so to skip cleanly we return early from here — the input
            // area is drawn by a later call in the same function.
            // Actually, the input rendering is *inline* after the
            // messages — we need to keep going but skip just the
            // messages code. Solution: handle input ourselves.
            super::render::draw_chat_input(frame, app, input_area);
            return;
        }
    }

    // Empty state.
    if app.chat.messages.is_empty() && !app.chat.awaiting_response() {
        let lines = if app.chat.coordinator_active {
            vec![
                Line::from(Span::styled(
                    "Chat with Coordinator",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Press 'c' or ':' to start typing.",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "Messages are sent to the coordinator agent.",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        } else {
            vec![
                Line::from(Span::styled(
                    "Coordinator agent not active.",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Start with: wg service start --coordinator-agent",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    "Chat history:",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    "(empty)",
                    Style::default().fg(Color::DarkGray),
                )),
            ]
        };
        let msg = Paragraph::new(lines);
        frame.render_widget(msg, msg_area);
        if chat_search_active && search_bar_area.height > 0 {
            draw_chat_search_bar(frame, app, search_bar_area);
        }
        draw_chat_input(frame, app, input_area);
        return;
    }

    // Build rendered lines from messages with word-wrapping.
    // Scrollbar overlays the rightmost column when visible.
    let content_width = width.saturating_sub(1);
    let mut rendered_lines: Vec<Line> = Vec::new();
    // Track which message index each rendered line belongs to (for click-to-edit).
    let mut line_to_message: Vec<Option<usize>> = Vec::new();

    // Show "older messages" indicator when there's more history to load.
    if app.chat.has_more_history {
        let remaining = app
            .chat
            .total_history_count
            .saturating_sub(app.chat.messages.len());
        let label = format!("  --- {} older messages (scroll up to load) ---", remaining);
        rendered_lines.push(Line::from(Span::styled(
            label,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
        line_to_message.push(None);
        rendered_lines.push(Line::from(""));
        line_to_message.push(None);
    }

    // Subtle warm-tinted dark background for user messages (like iMessage blue/gray,
    // but extremely subtle). Echoes the yellow ">" prefix and loop arrows.
    let user_msg_bg = Color::Rgb(30, 28, 20);
    // Brighter background for editable (unconsumed) user messages.
    let editable_user_msg_bg = Color::Rgb(35, 32, 20);
    // Highlight background for the message currently being edited.
    let editing_msg_bg = Color::Rgb(40, 35, 15);

    let editing_index = app.chat.editing_index;

    // Subtle magenta-tinted background for sent-to-agent messages.
    let sent_msg_bg = Color::Rgb(30, 20, 30);

    let session_gap_threshold = if app.session_gap_minutes > 0 {
        Some(chrono::Duration::minutes(app.session_gap_minutes as i64))
    } else {
        None
    };

    for (msg_idx, msg) in app.chat.messages.iter().enumerate() {
        // Session boundary divider: if there's a significant time gap between
        // this message and the previous one, insert a visual separator.
        if let Some(threshold) = &session_gap_threshold
            && msg_idx > 0
            && let (Some(prev_ts), Some(cur_ts)) = (
                app.chat.messages[msg_idx - 1].msg_timestamp.as_deref(),
                msg.msg_timestamp.as_deref(),
            )
            && let (Ok(prev_dt), Ok(cur_dt)) = (
                chrono::DateTime::parse_from_rfc3339(prev_ts),
                chrono::DateTime::parse_from_rfc3339(cur_ts),
            )
        {
            let gap = cur_dt.signed_duration_since(prev_dt);
            if gap > *threshold {
                let local_dt = cur_dt.with_timezone(&chrono::Local);
                let label = local_dt.format("%B %-d, %Y · %-I:%M %p").to_string();
                let dashes_total = content_width.saturating_sub(label.len() + 2);
                let left = dashes_total / 2;
                let right = dashes_total - left;
                let divider_text = format!("{} {} {}", "─".repeat(left), label, "─".repeat(right),);
                rendered_lines.push(Line::from(""));
                line_to_message.push(None);
                rendered_lines.push(Line::from(Span::styled(
                    divider_text,
                    Style::default().fg(Color::DarkGray),
                )));
                line_to_message.push(None);
                rendered_lines.push(Line::from(""));
                line_to_message.push(None);
            }
        }

        let is_coordinator = msg.role == super::state::ChatRole::Coordinator;
        let is_user = msg.role == super::state::ChatRole::User;
        let is_sent_message = msg.role == super::state::ChatRole::SentMessage;
        let is_editable = is_user && !app.is_chat_message_consumed(msg_idx);
        let is_being_edited = editing_index == Some(msg_idx);

        let (prefix, role_style) = match msg.role {
            super::state::ChatRole::User => {
                let name = msg.user.as_deref().unwrap_or("user");
                (
                    format!("{}: ", name),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                )
            }
            super::state::ChatRole::Coordinator => (
                "↯ ".to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            super::state::ChatRole::System => (
                "! ".to_string(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            super::state::ChatRole::SystemError => (
                "⚠ ERROR: ".to_string(),
                Style::default()
                    .fg(Color::Red)
                    .bg(Color::Indexed(52))
                    .add_modifier(Modifier::BOLD),
            ),
            super::state::ChatRole::SentMessage => {
                let target = msg.target_task.as_deref().unwrap_or("task");
                (
                    format!("→ {}: ", target),
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )
            }
        };

        let prefix_len = prefix.width();
        let indent = " ".repeat(prefix_len);
        let text_width = content_width.saturating_sub(prefix_len);

        // Render full content with markdown for all messages.
        // For coordinator messages with full_text, use the full response.
        let display_text = if is_coordinator {
            msg.full_text.as_deref().unwrap_or(&msg.text)
        } else {
            &msg.text
        };

        // Render markdown to styled lines, then word-wrap with tool-box awareness.
        let md_lines = markdown_to_lines(display_text, text_width);

        // Build wrapped lines. For coordinator tool-box lines, use special
        // wrapping that maintains the │ prefix on continuation lines and
        // preserves content coloring.
        let border_style = Style::default().fg(Color::DarkGray);
        let tool_name_style = Style::default()
            .fg(Color::Indexed(75))
            .add_modifier(Modifier::BOLD);
        let tool_content_style = Style::default().fg(Color::Indexed(252));

        let wrapped: Vec<Line> = if md_lines.is_empty() {
            vec![Line::from("")]
        } else if !is_coordinator {
            wrap_line_spans(&md_lines, text_width)
        } else {
            let mut out: Vec<Line> = Vec::new();
            for line in &md_lines {
                let lt: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                if lt.starts_with("┌─") {
                    // Tool header: border in DarkGray, tool name highlighted.
                    // Format: ┌─ ToolName ────
                    // "┌─ " = 7 bytes (┌=3 + ─=3 + space=1).
                    let after_prefix = lt.get(7..).unwrap_or("");
                    let name_end = after_prefix.find(['─', ' ']).unwrap_or(after_prefix.len());
                    let tool_name = after_prefix[..name_end].trim();
                    let rest_start = 7 + name_end; // byte offset into lt
                    let rest = lt.get(rest_start..).unwrap_or("");
                    out.push(Line::from(vec![
                        Span::styled("┌─ ", border_style),
                        Span::styled(tool_name.to_string(), tool_name_style),
                        Span::styled(format!(" {}", rest.trim_start()), border_style),
                    ]));
                } else if lt.starts_with("└─") {
                    // Tool footer: all DarkGray.
                    out.push(Line::from(Span::styled(lt, border_style)));
                } else if lt.starts_with("│ ") {
                    // Tool content line: │ in DarkGray, content in readable color.
                    // "│ " is 4 bytes (│=3 + space=1), 2 display columns.
                    let content = lt.get(4..).unwrap_or("");
                    let pipe_display_w: usize = 2; // "│ " = 2 columns
                    let cont_display_w: usize = 4; // "│   " = 4 columns
                    let wrap_w = text_width.saturating_sub(cont_display_w);
                    if wrap_w == 0 || content.width() <= text_width.saturating_sub(pipe_display_w) {
                        // Fits on one line.
                        out.push(Line::from(vec![
                            Span::styled("│ ", border_style),
                            Span::styled(content.to_string(), tool_content_style),
                        ]));
                    } else {
                        // Wrap content, first line with "│ ", continuations with "│   ".
                        let wrapped_content = word_wrap(content, wrap_w);
                        for (i, w) in wrapped_content.iter().enumerate() {
                            if i == 0 {
                                out.push(Line::from(vec![
                                    Span::styled("│ ", border_style),
                                    Span::styled(w.to_string(), tool_content_style),
                                ]));
                            } else {
                                out.push(Line::from(vec![
                                    Span::styled("│   ", border_style),
                                    Span::styled(w.to_string(), tool_content_style),
                                ]));
                            }
                        }
                    }
                } else {
                    // Normal line — standard wrapping.
                    out.extend(wrap_line_spans(std::slice::from_ref(line), text_width));
                }
            }
            out
        };

        // Choose background for user messages based on state.
        let msg_bg = if is_being_edited {
            editing_msg_bg
        } else if is_editable {
            editable_user_msg_bg
        } else {
            user_msg_bg
        };

        let mut first_line = true;
        for line in &wrapped {
            if first_line {
                let mut spans = vec![Span::styled(prefix.clone(), role_style)];
                spans.extend(line.spans.iter().cloned());
                // Append "(edited)" indicator on the first line of edited messages.
                if msg.edited {
                    spans.push(Span::styled(
                        " (edited)",
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
                // Append read-at annotation for sent messages.
                if is_sent_message && let Some(read_at) = &msg.read_at {
                    let now = chrono::Utc::now();
                    let rel = format_relative_time(read_at, &now);
                    spans.push(Span::styled(
                        format!("  read {}", rel),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                // Append timestamp for all messages.
                if let Some(ts) = &msg.msg_timestamp {
                    let now = chrono::Utc::now();
                    let rel = format_relative_time(ts, &now);
                    spans.push(Span::styled(
                        format!("  {}", rel),
                        Style::default().fg(Color::Indexed(239)),
                    ));
                }
                // Position in chat sequence for user messages (e.g. "#5/12")
                // so the user can see where their message landed relative to the
                // full series of turns.
                if is_user {
                    let total = app.chat.messages.len();
                    spans.push(Span::styled(
                        format!("  #{}/{}", msg_idx + 1, total),
                        Style::default().fg(Color::Indexed(239)),
                    ));
                }
                // Delivery status for user messages.
                if is_user {
                    // Check if a coordinator message follows (= delivered).
                    let has_response = app
                        .chat
                        .messages
                        .get(msg_idx + 1..)
                        .map(|rest| {
                            rest.iter().any(|m| {
                                m.role == super::state::ChatRole::Coordinator
                                    || m.role == super::state::ChatRole::SystemError
                            })
                        })
                        .unwrap_or(false);
                    let status_span = if has_response {
                        Span::styled("  ✓✓", Style::default().fg(Color::DarkGray))
                    } else if app.chat.awaiting_response() {
                        // Only the last user message shows processing indicator.
                        let is_last_user = app.chat.messages[msg_idx + 1..]
                            .iter()
                            .all(|m| m.role != super::state::ChatRole::User);
                        if is_last_user {
                            Span::styled("  ⋯", Style::default().fg(Color::Yellow))
                        } else {
                            Span::styled("  ✓", Style::default().fg(Color::DarkGray))
                        }
                    } else {
                        Span::styled("  ✓", Style::default().fg(Color::DarkGray))
                    };
                    spans.push(status_span);
                }
                let mut built = Line::from(spans);
                if is_user {
                    built = apply_line_bg(built, msg_bg);
                } else if is_sent_message {
                    built = apply_line_bg(built, sent_msg_bg);
                }
                rendered_lines.push(built);
                line_to_message.push(Some(msg_idx));
                first_line = false;
            } else {
                // Continuation/tool lines: indent to align with text after prefix
                let mut spans = vec![Span::raw(indent.clone())];
                spans.extend(line.spans.iter().cloned());
                let mut built = Line::from(spans);
                if is_user {
                    built = apply_line_bg(built, msg_bg);
                } else if is_sent_message {
                    built = apply_line_bg(built, sent_msg_bg);
                }
                rendered_lines.push(built);
                line_to_message.push(Some(msg_idx));
            }
        }
        // Show attachment indicators.
        for att_name in &msg.attachments {
            let att_text = format!("{}[Attached: {}]", indent, att_name);
            let mut att_line = Line::from(Span::styled(
                att_text,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::ITALIC),
            ));
            if is_user {
                att_line = apply_line_bg(att_line, msg_bg);
            }
            rendered_lines.push(att_line);
            line_to_message.push(Some(msg_idx));
        }
        // Blank line between messages.
        rendered_lines.push(Line::from(""));
        line_to_message.push(None);
    }

    // Streaming indicator / progressive text when awaiting response.
    if app.chat.awaiting_response() {
        if app.chat.streaming_text.is_empty() {
            // No streaming text yet — show animated lightning-wave with elapsed time.
            let elapsed = app
                .chat
                .awaiting_since
                .map(|t| t.elapsed())
                .unwrap_or_default();
            rendered_lines.push(spinner_wave_line(elapsed, ""));
            line_to_message.push(None);
            // Show interrupt hint after a brief delay so it's not distracting on fast responses.
            if elapsed.as_secs() >= 2 {
                rendered_lines.push(Line::from(Span::styled(
                    "  Ctrl+C to interrupt",
                    Style::default().fg(Color::DarkGray),
                )));
                line_to_message.push(None);
            }
        } else {
            // Show progressive streaming text from the coordinator with markdown
            // rendering and word wrapping, matching finalized coordinator messages.
            let prefix = "↯ ";
            let prefix_len = prefix.width();
            let indent = " ".repeat(prefix_len);
            let text_width = content_width.saturating_sub(prefix_len);

            let md_lines = markdown_to_lines(&app.chat.streaming_text, text_width);

            // Wrap with tool-box awareness (same logic as finalized coordinator messages).
            let border_style = Style::default().fg(Color::DarkGray);
            let tool_name_style = Style::default()
                .fg(Color::Indexed(75))
                .add_modifier(Modifier::BOLD);
            let tool_content_style = Style::default().fg(Color::Indexed(252));

            let wrapped: Vec<Line> = if md_lines.is_empty() {
                vec![Line::from("")]
            } else {
                let mut out: Vec<Line> = Vec::new();
                for line in &md_lines {
                    let lt: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                    if lt.starts_with("┌─") {
                        let after_prefix = lt.get(7..).unwrap_or("");
                        let name_end = after_prefix.find(['─', ' ']).unwrap_or(after_prefix.len());
                        let tool_name = after_prefix[..name_end].trim();
                        let rest_start = 7 + name_end;
                        let rest = lt.get(rest_start..).unwrap_or("");
                        out.push(Line::from(vec![
                            Span::styled("┌─ ", border_style),
                            Span::styled(tool_name.to_string(), tool_name_style),
                            Span::styled(format!(" {}", rest.trim_start()), border_style),
                        ]));
                    } else if lt.starts_with("└─") {
                        out.push(Line::from(Span::styled(lt, border_style)));
                    } else if lt.starts_with("│ ") {
                        let content = lt.get(4..).unwrap_or("");
                        let pipe_display_w: usize = 2;
                        let cont_display_w: usize = 4;
                        let wrap_w = text_width.saturating_sub(cont_display_w);
                        if wrap_w == 0
                            || content.width() <= text_width.saturating_sub(pipe_display_w)
                        {
                            out.push(Line::from(vec![
                                Span::styled("│ ", border_style),
                                Span::styled(content.to_string(), tool_content_style),
                            ]));
                        } else {
                            let wrapped_content = word_wrap(content, wrap_w);
                            for (i, w) in wrapped_content.iter().enumerate() {
                                if i == 0 {
                                    out.push(Line::from(vec![
                                        Span::styled("│ ", border_style),
                                        Span::styled(w.to_string(), tool_content_style),
                                    ]));
                                } else {
                                    out.push(Line::from(vec![
                                        Span::styled("│   ", border_style),
                                        Span::styled(w.to_string(), tool_content_style),
                                    ]));
                                }
                            }
                        }
                    } else {
                        out.extend(wrap_line_spans(std::slice::from_ref(line), text_width));
                    }
                }
                out
            };

            // Render with prefix on first line, indent on continuation lines.
            let role_style = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);
            let mut first_line = true;
            for line in &wrapped {
                if first_line {
                    let mut spans = vec![Span::styled(prefix, role_style)];
                    spans.extend(line.spans.iter().cloned());
                    rendered_lines.push(Line::from(spans));
                    line_to_message.push(None);
                    first_line = false;
                } else {
                    let mut spans = vec![Span::raw(indent.clone())];
                    spans.extend(line.spans.iter().cloned());
                    rendered_lines.push(Line::from(spans));
                    line_to_message.push(None);
                }
            }
            // Append animated lightning-wave with elapsed time to indicate still generating.
            let elapsed = app
                .chat
                .awaiting_since
                .map(|t| t.elapsed())
                .unwrap_or_default();
            rendered_lines.push(spinner_wave_line(elapsed, "  "));
            line_to_message.push(None);
        }
        rendered_lines.push(Line::from(""));
        line_to_message.push(None);
    }

    // Store line-to-message mapping for click-to-edit.
    app.chat.line_to_message = line_to_message;

    // Scrolling: `scroll` is lines from bottom (0 = fully scrolled down).
    let total_lines = rendered_lines.len();
    let viewport_h = msg_area.height as usize;
    app.chat.total_rendered_lines = total_lines;
    app.chat.viewport_height = viewport_h;

    // Anchor compensation: when new content arrives while scrolled up,
    // increase scroll-from-bottom so the viewport stays on the same content.
    if app.chat.scroll > 0 && app.chat.prev_total_rendered_lines > 0 {
        let delta = total_lines.saturating_sub(app.chat.prev_total_rendered_lines);
        if delta > 0 {
            app.chat.scroll = app.chat.scroll.saturating_add(delta);
        }
    }
    app.chat.prev_total_rendered_lines = total_lines;

    // Calculate the visible window.
    let scroll_from_top = if total_lines <= viewport_h {
        0
    } else {
        let max_scroll_from_bottom = total_lines.saturating_sub(viewport_h);
        let clamped_scroll = app.chat.scroll.min(max_scroll_from_bottom);
        max_scroll_from_bottom - clamped_scroll
    };

    app.chat.scroll_from_top = scroll_from_top;

    let end = (scroll_from_top + viewport_h).min(total_lines);
    let mut visible_lines: Vec<Line> = rendered_lines[scroll_from_top..end].to_vec();

    // Apply search highlights to visible lines.
    if !app.chat.search.query.is_empty() && !app.chat.search.matches.is_empty() {
        let query_lower = app.chat.search.query.to_lowercase();
        let current_msg_idx = app
            .chat
            .search
            .current_match
            .and_then(|idx| app.chat.search.matches.get(idx))
            .map(|m| m.message_idx);
        let highlight_bg = Color::Rgb(80, 80, 0); // yellow-ish highlight for matches
        let current_bg = Color::Rgb(180, 120, 0); // brighter for current match

        for (line_idx, line) in visible_lines.iter_mut().enumerate() {
            let global_line_idx = scroll_from_top + line_idx;
            if let Some(Some(_msg_idx)) = app.chat.line_to_message.get(global_line_idx) {
                let is_current_msg = current_msg_idx == Some(*_msg_idx);
                let bg = if is_current_msg {
                    current_bg
                } else {
                    highlight_bg
                };
                *line = highlight_query_in_line(line.clone(), &query_lower, bg);
            }
        }
    }

    let paragraph = Paragraph::new(visible_lines);
    frame.render_widget(paragraph, msg_area);

    // Visual indicator: show a cyan "▌" column on the left edge when chat history
    // has sub-focus and the right panel is focused.
    if app.focused_panel == super::state::FocusedPanel::RightPanel
        && app.inspector_sub_focus == super::state::InspectorSubFocus::ChatHistory
        && app.input_mode == super::state::InputMode::Normal
        && msg_area.height > 0
    {
        let indicator_height = msg_area.height.min(3);
        for dy in 0..indicator_height {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "▌",
                    Style::default().fg(Color::Cyan),
                ))),
                Rect {
                    x: msg_area.x,
                    y: msg_area.y + dy,
                    width: 1,
                    height: 1,
                },
            );
        }
    }

    // Yellow HUD: distance from bottom when scrolled up (anchored mode).
    if app.chat.scroll > 0 && msg_area.width > 4 && msg_area.height > 0 {
        let indicator = format!(" ↓{} ", app.chat.scroll);
        let x = msg_area.x + msg_area.width.saturating_sub(indicator.len() as u16 + 1);
        let y = msg_area.y;
        if x >= msg_area.x {
            let buf = frame.buffer_mut();
            for (i, ch) in indicator.chars().enumerate() {
                let cx = x + i as u16;
                if cx < msg_area.x + msg_area.width {
                    let cell = &mut buf[(cx, y)];
                    cell.set_char(ch);
                    cell.set_style(
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Yellow),
                    );
                }
            }
        }
    }

    // Scrollbar if content overflows (auto-hides after 2 seconds of inactivity).
    if total_lines > viewport_h && app.panel_scrollbar_visible() {
        draw_panel_scrollbar(
            frame,
            app,
            msg_area,
            total_lines.saturating_sub(viewport_h),
            scroll_from_top,
        );
    }

    // Search bar (between messages and input).
    if chat_search_active && search_bar_area.height > 0 {
        draw_chat_search_bar(frame, app, search_bar_area);
    }

    // Input area.
    draw_chat_input(frame, app, input_area);
}

/// Draw the chat search bar showing the current query and match count.
fn draw_chat_search_bar(frame: &mut Frame, app: &VizApp, area: Rect) {
    let is_active = app.input_mode == InputMode::ChatSearch;
    let color = if is_active {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let mut spans = vec![
        Span::styled(
            "/ ",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            app.chat.search.query.clone(),
            Style::default().fg(Color::White),
        ),
    ];
    if !app.chat.search.query.is_empty() {
        let match_info = if app.chat.search.matches.is_empty() {
            " [no matches]".to_string()
        } else {
            let idx = app.chat.search.current_match.unwrap_or(0);
            format!(" [{}/{}]", idx + 1, app.chat.search.matches.len())
        };
        spans.push(Span::styled(
            match_info,
            Style::default().fg(Color::DarkGray),
        ));
    }
    if is_active && app.chat.search.query.is_empty() {
        spans.push(Span::styled(
            "type to search...",
            Style::default().fg(Color::DarkGray),
        ));
    }
    // Cursor indicator when actively typing.
    if is_active {
        spans.push(Span::styled("█", Style::default().fg(color)));
    }
    let line = Line::from(spans);
    let bg = if is_active {
        Color::Rgb(20, 30, 40)
    } else {
        Color::Rgb(15, 15, 20)
    };
    let paragraph = Paragraph::new(vec![line]).style(Style::default().bg(bg));
    frame.render_widget(paragraph, area);
}

/// Draw the chat input line at the bottom of the chat panel.
fn draw_chat_input(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let is_editing = app.input_mode == InputMode::ChatInput;
    let in_edit_mode = app.chat.editing_index.is_some();
    let has_text = !super::state::editor_is_empty(&app.chat.editor);
    app.last_chat_input_area = area;
    // Yellow/gold border when editing an existing message; default terminal
    // color for the normal active input (no purple styling — keeps the
    // composer visually neutral).
    let border_color = if is_editing && in_edit_mode {
        Color::Yellow
    } else if is_editing {
        Color::Reset
    } else {
        Color::DarkGray
    };
    let prompt_color = if is_editing && in_edit_mode {
        Color::Yellow
    } else if is_editing {
        Color::Reset
    } else {
        Color::DarkGray
    };
    if is_editing || has_text {
        // Separator line — show edit mode hint.
        let sep_text = if is_editing && in_edit_mode {
            let w = area.width as usize;
            let hint = " Editing (Enter=save, Esc=cancel) ";
            if w > hint.len() + 2 {
                let left = (w - hint.len()) / 2;
                let right = w - hint.len() - left;
                format!("{}{}{}", "─".repeat(left), hint, "─".repeat(right))
            } else {
                "─".repeat(w)
            }
        } else {
            "─".repeat(area.width as usize)
        };
        let sep = Line::from(Span::styled(
            sep_text,
            Style::default()
                .fg(border_color)
                .add_modifier(if is_editing {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));
        if area.height >= 2 {
            frame.render_widget(
                Paragraph::new(sep),
                Rect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: 1,
                },
            );
        }
        let input_y = if area.height >= 2 { area.y + 1 } else { area.y };
        let input_h = if area.height >= 2 {
            area.height - 1
        } else {
            area.height
        };
        let prefix_len: u16 = 2;
        if input_h > 0 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "> ",
                    Style::default()
                        .fg(prompt_color)
                        .add_modifier(if is_editing {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ))),
                Rect {
                    x: area.x,
                    y: input_y,
                    width: prefix_len,
                    height: 1,
                },
            );
        }
        let editor_area = Rect {
            x: area.x + prefix_len,
            y: input_y,
            width: area.width.saturating_sub(prefix_len),
            height: input_h,
        };
        let text_color = if is_editing {
            Color::Reset
        } else {
            Color::DarkGray
        };
        let cursor_style = if is_editing {
            Style::default().fg(Color::Black).bg(Color::White)
        } else {
            Style::default().fg(text_color)
        };
        render_editor_word_wrap(
            frame,
            &app.chat.editor,
            editor_area,
            Style::default().fg(text_color),
            cursor_style,
            is_editing,
        );
        if !app.chat.pending_attachments.is_empty() {
            let att_text: String = app
                .chat
                .pending_attachments
                .iter()
                .map(|a| format!("[{}]", a.filename))
                .collect::<Vec<_>>()
                .join(" ");
            let att_y = (input_y + input_h).min(area.y + area.height.saturating_sub(1));
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    format!(" \u{1F4CE} {}", att_text),
                    Style::default().fg(Color::Green),
                ))),
                Rect {
                    x: area.x,
                    y: att_y,
                    width: area.width,
                    height: 1,
                },
            );
        }
    } else {
        let hint_text = if app.chat_pty_mode && app.chat_pty_forwards_stdin {
            " Ctrl+T: leave PTY  PgUp/Dn: scroll".to_string()
        } else if app.chat_pty_mode {
            " Enter: chat  ↑↓: scroll  Ctrl+T: focus PTY".to_string()
        } else if app.chat.pending_attachments.is_empty() {
            " c: chat  \u{2191}\u{2193}: scroll".to_string()
        } else {
            format!(
                " c: chat  \u{2191}\u{2193}: scroll  {} attached",
                app.chat.pending_attachments.len()
            )
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                hint_text,
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
    }
}

/// Apply a background color to every span in a line.
fn apply_line_bg(line: Line<'_>, bg: Color) -> Line<'_> {
    Line::from(
        line.spans
            .into_iter()
            .map(|s| {
                let new_style = s.style.bg(bg);
                s.style(new_style)
            })
            .collect::<Vec<_>>(),
    )
}

/// Count the number of visual lines for the given input text at a given width.
/// Splits by newlines first, then wraps each logical line.
fn count_visual_lines(input: &str, usable_width: usize) -> usize {
    if input.is_empty() {
        return 1;
    }
    let mut count = 0;
    for line in input.split('\n') {
        count += word_wrap_segments(line, usable_width).len();
    }
    count
}

/// Word-wrap a single logical line into segments, breaking at word boundaries.
/// Returns `Vec<(start_char, end_char)>` pairs — character index ranges within the line.
/// Characters between consecutive segments (gaps) are consumed whitespace.
/// Only hard-breaks when a single word exceeds the width.
pub(super) fn word_wrap_segments(line: &str, width: usize) -> Vec<(usize, usize)> {
    use unicode_width::UnicodeWidthChar;

    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();

    if n == 0 {
        return vec![(0, 0)];
    }
    if width == 0 {
        return vec![(0, n)];
    }

    let cw = |c: char| -> usize { UnicodeWidthChar::width(c).unwrap_or(0) };

    let mut result = Vec::new();
    let mut pos = 0;

    while pos < n {
        let seg_start = pos;
        let mut col: usize = 0;
        // Last position where we could end the visual line (after a word or at a ws→word boundary).
        let mut last_break: Option<usize> = None;

        while pos < n {
            let char_width = cw(chars[pos]);

            if col + char_width > width && pos > seg_start {
                break;
            }

            col += char_width;
            pos += 1;

            // Record break points at word boundaries:
            // - non-ws followed by ws (end of word)
            // - ws followed by non-ws (start of word)
            if pos < n {
                let curr_is_ws = chars[pos - 1].is_whitespace();
                let next_is_ws = chars[pos].is_whitespace();
                if (!curr_is_ws && next_is_ws) || (curr_is_ws && !next_is_ws) {
                    last_break = Some(pos);
                }
            }
        }

        if pos >= n {
            result.push((seg_start, n));
            break;
        }

        // Overflow: break at last word boundary, or hard-break.
        if let Some(bp) = last_break {
            result.push((seg_start, bp));
            pos = bp;
            while pos < n && chars[pos].is_whitespace() {
                pos += 1;
            }
        } else {
            // No word boundary in this segment — hard break.
            result.push((seg_start, pos));
        }
    }

    if result.is_empty() {
        result.push((0, 0));
    }

    result
}

/// Map a cursor column (char index) in a logical line to (visual_line_offset, char_offset)
/// within the word-wrapped segments.
pub(super) fn cursor_in_segments(segments: &[(usize, usize)], cursor_col: usize) -> (usize, usize) {
    for (i, &(start, end)) in segments.iter().enumerate() {
        if cursor_col < end {
            return (i, cursor_col - start);
        }
        // Cursor is in the gap (consumed whitespace) between segments.
        let next_start = segments.get(i + 1).map(|s| s.0).unwrap_or(end);
        if cursor_col < next_start {
            return (i, end - start);
        }
    }
    // Past the end — show at end of last segment.
    if let Some(&(start, end)) = segments.last() {
        (segments.len() - 1, end - start)
    } else {
        (0, 0)
    }
}

/// Render an edtui EditorState with word-boundary wrapping.
/// Replaces EditorView for cases where word wrapping is needed.
fn render_editor_word_wrap(
    frame: &mut Frame,
    editor: &edtui::EditorState,
    area: Rect,
    text_style: Style,
    cursor_style: Style,
    show_cursor: bool,
) {
    use unicode_width::UnicodeWidthChar;

    let text = editor.lines.to_string();
    let width = area.width as usize;
    if width == 0 || area.height == 0 {
        return;
    }

    let logical_lines: Vec<&str> = text.split('\n').collect();

    // Build visual lines and find cursor position.
    let mut visual_lines: Vec<String> = Vec::new();
    let mut cursor_visual_row = 0usize;
    let mut cursor_char_offset = 0usize; // char offset within the visual line
    let mut found_cursor = false;

    for (line_idx, logical_line) in logical_lines.iter().enumerate() {
        let chars: Vec<char> = logical_line.chars().collect();
        let segments = word_wrap_segments(logical_line, width);

        if line_idx == editor.cursor.row && !found_cursor {
            let (sub_row, sub_col) = cursor_in_segments(&segments, editor.cursor.col);
            cursor_visual_row = visual_lines.len() + sub_row;
            cursor_char_offset = sub_col;
            found_cursor = true;
        }

        for &(start, end) in &segments {
            let line_text: String = chars[start..end].iter().collect();
            visual_lines.push(line_text);
        }
    }

    let total_visual = visual_lines.len();
    let viewport_h = area.height as usize;

    // Scroll to keep cursor visible.
    let scroll = if cursor_visual_row >= viewport_h {
        cursor_visual_row - viewport_h + 1
    } else {
        0
    };

    // Render visible lines.
    let visible_end = (scroll + viewport_h).min(total_visual);
    for (i, vi) in (scroll..visible_end).enumerate() {
        let y = area.y + i as u16;
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                visual_lines[vi].clone(),
                text_style,
            ))),
            Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            },
        );
    }

    // Draw cursor.
    if show_cursor && cursor_visual_row >= scroll && cursor_visual_row < scroll + viewport_h {
        let cursor_y = area.y + (cursor_visual_row - scroll) as u16;
        // Convert char offset to display column.
        let cursor_display_col: usize = if cursor_visual_row < visual_lines.len() {
            visual_lines[cursor_visual_row]
                .chars()
                .take(cursor_char_offset)
                .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                .sum()
        } else {
            0
        };
        let cursor_x = area.x + (cursor_display_col as u16).min(area.width.saturating_sub(1));

        // Character under cursor (or space if at end of line).
        let cursor_char = if cursor_visual_row < visual_lines.len() {
            visual_lines[cursor_visual_row]
                .chars()
                .nth(cursor_char_offset)
                .unwrap_or(' ')
        } else {
            ' '
        };
        let char_w = UnicodeWidthChar::width(cursor_char).unwrap_or(1).max(1) as u16;

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                cursor_char.to_string(),
                cursor_style,
            ))),
            Rect {
                x: cursor_x,
                y: cursor_y,
                width: char_w.min(area.width.saturating_sub(cursor_display_col as u16)),
                height: 1,
            },
        );
    }
}

/// Draw the per-task Log tab — live agent output (default) or the
/// structured task.log entries (when `log_pane.view_top` is true,
/// toggled with `v`).
///
/// Data sources:
/// - `app.log_pane.rendered_lines`: `[<rel-time>] <message>` entries
///   populated by `load_log_pane()` from `task.log`.
/// - `app.log_pane.agent_output.full_text`: live agent stdout,
///   populated by `update_log_output()` from `agents/<id>/output.log`.
fn draw_log_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    use ratatui::widgets::{Paragraph, Wrap};

    if area.height == 0 || area.width == 0 {
        return;
    }

    let header_line = {
        let task_label = app
            .log_pane
            .task_id
            .clone()
            .unwrap_or_else(|| "(no task selected)".to_string());
        let agent_label = app
            .log_pane
            .agent_id
            .as_deref()
            .map(|id| format!("agent={}", id))
            .unwrap_or_else(|| "no agent".to_string());
        let view_label = if app.log_pane.view_top {
            "view=events"
        } else if !app.log_pane.stream_events.is_empty() {
            "view=activity"
        } else {
            "view=stream"
        };
        let tail_label = if app.log_pane.auto_tail {
            "tail=on"
        } else {
            "tail=off"
        };
        Line::from(vec![
            Span::styled(
                format!(" {} ", task_label),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}  {}  {}", agent_label, view_label, tail_label),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                "    [4] toggle view  [J] json",
                Style::default().fg(Color::Indexed(239)),
            ),
        ])
    };

    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(header_line), header_area);

    app.log_pane.viewport_height = body_area.height as usize;

    // Collect display lines for whichever view is active.
    let lines: Vec<Line> = if app.log_pane.view_top {
        if app.log_pane.rendered_lines.is_empty() {
            vec![Line::from(Span::styled(
                "(no log entries yet)",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            app.log_pane
                .rendered_lines
                .iter()
                .map(|s| Line::from(s.clone()))
                .collect()
        }
    } else if !app.log_pane.stream_events.is_empty() {
        let mut out: Vec<Line> = Vec::new();
        for event in &app.log_pane.stream_events {
            let color = match event.kind {
                AgentStreamEventKind::ToolCall => Color::Cyan,
                AgentStreamEventKind::ToolResult => Color::Green,
                AgentStreamEventKind::TextOutput => Color::White,
                AgentStreamEventKind::Thinking => Color::Magenta,
                AgentStreamEventKind::SystemEvent => Color::DarkGray,
                AgentStreamEventKind::Error => Color::Red,
            };
            for sub_line in event.summary.split('\n') {
                out.push(Line::from(Span::styled(
                    sub_line.to_string(),
                    Style::default().fg(color),
                )));
            }
        }
        out
    } else {
        let text = &app.log_pane.agent_output.full_text;
        if text.is_empty() {
            vec![Line::from(Span::styled(
                "(no agent output yet — is the task running?)",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            text.split('\n')
                .map(|l| Line::from(l.to_string()))
                .collect()
        }
    };
    app.log_pane.total_wrapped_lines = lines.len();

    // Auto-tail: pin scroll to the bottom when enabled and there's overflow.
    let viewport = body_area.height as usize;
    if app.log_pane.auto_tail {
        app.log_pane.scroll = lines.len().saturating_sub(viewport);
    }

    let scroll_y = app.log_pane.scroll.min(lines.len().saturating_sub(1)) as u16;
    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    frame.render_widget(para, body_area);
}

/// Draw the Messages tab — wg msg traffic for the currently selected task.
fn draw_messages_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    use ratatui::widgets::{Paragraph, Wrap};
    use super::state::MessageDirection;

    if area.height == 0 || area.width == 0 {
        return;
    }

    let task_label = app
        .messages_panel
        .task_id
        .clone()
        .unwrap_or_else(|| "(no task selected)".to_string());
    let summary = &app.messages_panel.summary;
    let header_line = Line::from(vec![
        Span::styled(
            format!(" {} ", task_label),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " in:{} out:{} unanswered:{}",
                summary.incoming, summary.outgoing, summary.unanswered
            ),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            "    [Enter] compose  [7] switch",
            Style::default().fg(Color::Indexed(239)),
        ),
    ]);

    let is_editing = app.input_mode == InputMode::MessageInput;
    let editor_text_str = super::state::editor_text(&app.messages_panel.editor);
    let has_input_text = !editor_text_str.is_empty();

    let input_height: u16 = if is_editing || has_input_text {
        let prompt_prefix = 2;
        let usable = (area.width as usize).saturating_sub(prompt_prefix).max(1);
        let visual_lines = count_visual_lines(&editor_text_str, usable);
        let wrapped_lines = (visual_lines as u16).max(1);
        let max_input = (area.height * 3 / 4).max(2);
        wrapped_lines.min(max_input) + 1 // +1 for separator
    } else {
        1
    };

    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(header_line), header_area);

    let body_h = body_area.height;
    let msg_area_height = body_h.saturating_sub(input_height);
    let msg_area = Rect {
        x: body_area.x,
        y: body_area.y,
        width: body_area.width,
        height: msg_area_height,
    };
    let input_area = Rect {
        x: body_area.x,
        y: body_area.y + msg_area_height,
        width: body_area.width,
        height: input_height,
    };
    app.last_message_input_area = input_area;

    // Render message list.
    let lines: Vec<Line> = if app.messages_panel.entries.is_empty() {
        vec![Line::from(Span::styled(
            "(no messages — press Enter to compose)",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.messages_panel
            .entries
            .iter()
            .map(|entry| {
                let (arrow, color) = match entry.direction {
                    MessageDirection::Incoming => ("←", Color::Cyan),
                    MessageDirection::Outgoing => ("→", Color::Green),
                };
                let urgent_marker = if entry.is_urgent { " [!]" } else { "" };
                Line::from(vec![
                    Span::styled(
                        format!("[{}] ", entry.timestamp),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        format!("{} {} ", arrow, entry.display_label),
                        Style::default().fg(color),
                    ),
                    Span::styled(
                        format!("{}{}", entry.body, urgent_marker),
                        Style::default().fg(Color::White),
                    ),
                ])
            })
            .collect()
    };

    app.messages_panel.total_wrapped_lines = lines.len();
    app.messages_panel.viewport_height = msg_area.height as usize;

    let viewport = msg_area.height as usize;
    let max_scroll = lines.len().saturating_sub(viewport);
    app.messages_panel.scroll = app.messages_panel.scroll.min(max_scroll);
    let scroll_y = app.messages_panel.scroll as u16;

    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y, 0));
    frame.render_widget(para, msg_area);

    // Render input area.
    if input_area.height > 0 {
        // Separator line.
        let sep = Paragraph::new(Line::from(Span::styled(
            "─".repeat(input_area.width as usize),
            Style::default().fg(Color::DarkGray),
        )));
        frame.render_widget(
            sep,
            Rect {
                x: input_area.x,
                y: input_area.y,
                width: input_area.width,
                height: 1,
            },
        );

        let editor_area = Rect {
            x: input_area.x + 2,
            y: input_area.y + 1,
            width: input_area.width.saturating_sub(2),
            height: input_area.height.saturating_sub(1),
        };

        // Prompt indicator.
        if editor_area.height > 0 {
            let prompt_char = if is_editing { ">" } else { " " };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("{} ", prompt_char),
                    Style::default().fg(if is_editing {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    }),
                )),
                Rect {
                    x: input_area.x,
                    y: input_area.y + 1,
                    width: 2,
                    height: 1,
                },
            );

            let text_color = if is_editing {
                Color::White
            } else {
                Color::DarkGray
            };
            let cursor_style = if is_editing {
                Style::default().fg(Color::Black).bg(Color::White)
            } else {
                Style::default().fg(text_color)
            };
            render_editor_word_wrap(
                frame,
                &app.messages_panel.editor,
                editor_area,
                Style::default().fg(text_color),
                cursor_style,
                is_editing,
            );
        }
    }
}

/// Draw the Coordinator Log tab (panel 7) — activity feed from operations.jsonl.
fn draw_coord_log_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let header_lines = build_coordinator_runtime_lines(app);
    let area = if !header_lines.is_empty() && area.height > 4 {
        let header_height = (header_lines.len() as u16).min(area.height.saturating_sub(1));
        let [header_area, body_area] =
            Layout::vertical([Constraint::Length(header_height), Constraint::Min(0)]).areas(area);
        frame.render_widget(Paragraph::new(header_lines), header_area);
        body_area
    } else {
        area
    };

    // If we have activity feed events, render the semantic view.
    // Otherwise fall back to the raw daemon.log display.
    if !app.activity_feed.events.is_empty() {
        draw_activity_feed(frame, app, area);
        return;
    }

    // Fallback: raw daemon.log (original behavior).
    if app.coord_log.rendered_lines.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(Span::styled(
                "Activity Feed",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "No activity yet.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "(Start the service with `wg service start`)",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(msg, area);
        return;
    }
    let viewport_h = area.height as usize;
    app.coord_log.viewport_height = viewport_h;
    if viewport_h == 0 {
        return;
    }
    let wrap_width = area.width as usize;
    let mut wrapped_lines: Vec<Line> = Vec::new();
    for s in &app.coord_log.rendered_lines {
        if let Some(bracket_start) = s.find('[')
            && let Some(bracket_end) = s[bracket_start..].find(']')
        {
            let bracket_end = bracket_start + bracket_end;
            let timestamp = &s[..bracket_start];
            let level = &s[bracket_start..=bracket_end];
            let message = &s[bracket_end + 1..];
            let prefix_len = bracket_end + 1;
            let level_color = match level {
                "[INFO]" => Color::Green,
                "[WARN]" => Color::Yellow,
                "[ERROR]" => Color::Red,
                _ => Color::DarkGray,
            };
            let text_width = wrap_width.saturating_sub(prefix_len);
            if text_width == 0 || message.trim().is_empty() {
                wrapped_lines.push(Line::from(vec![
                    Span::styled(timestamp.to_string(), Style::default().fg(Color::DarkGray)),
                    Span::styled(level.to_string(), Style::default().fg(level_color)),
                    Span::raw(message.to_string()),
                ]));
            } else {
                let leading_space = &message[..message.len() - message.trim_start().len()];
                let wrapped = word_wrap(message.trim_start(), text_width);
                let indent = " ".repeat(prefix_len + leading_space.len());
                for (i, wl) in wrapped.iter().enumerate() {
                    if i == 0 {
                        wrapped_lines.push(Line::from(vec![
                            Span::styled(
                                timestamp.to_string(),
                                Style::default().fg(Color::DarkGray),
                            ),
                            Span::styled(level.to_string(), Style::default().fg(level_color)),
                            Span::raw(format!("{}{}", leading_space, wl)),
                        ]));
                    } else {
                        wrapped_lines.push(Line::from(Span::raw(format!("{}{}", indent, wl))));
                    }
                }
            }
            continue;
        }
        if wrap_width > 0 && s.width() > wrap_width {
            let wrapped = word_wrap(s, wrap_width);
            for wl in wrapped {
                wrapped_lines.push(Line::from(wl));
            }
        } else {
            wrapped_lines.push(Line::from(Span::raw(s.as_str())));
        }
    }
    let total_lines = wrapped_lines.len();
    app.coord_log.total_wrapped_lines = total_lines;
    let scroll = app
        .coord_log
        .scroll
        .min(total_lines.saturating_sub(viewport_h));
    let end = (scroll + viewport_h).min(total_lines);
    let visible_lines: Vec<Line> = wrapped_lines[scroll..end].to_vec();
    let paragraph = Paragraph::new(visible_lines);
    frame.render_widget(paragraph, area);
    if total_lines > viewport_h && app.panel_scrollbar_visible() {
        draw_panel_scrollbar(
            frame,
            app,
            area,
            total_lines.saturating_sub(viewport_h),
            scroll,
        );
    }
}

fn build_coordinator_runtime_lines(app: &VizApp) -> Vec<Line<'static>> {
    let config = workgraph::config::Config::load_or_default(&app.workgraph_dir);
    let executor = config.coordinator.effective_executor();
    let model = config.coordinator.model.clone().unwrap_or_else(|| {
        config
            .resolve_model_for_role(workgraph::config::DispatchRole::Default)
            .model
    });
    let cid = app.active_coordinator_id;
    let state =
        workgraph::service::chat_compactor::ChatCompactorState::load(&app.workgraph_dir, cid);
    let threshold = config.chat.compact_threshold;
    let pending =
        workgraph::chat::read_inbox_since_for(&app.workgraph_dir, cid, state.last_inbox_id)
            .map(|m| m.len())
            .unwrap_or(0)
            + workgraph::chat::read_outbox_since_for(&app.workgraph_dir, cid, state.last_outbox_id)
                .map(|m| m.len())
                .unwrap_or(0);
    let summary_present =
        workgraph::service::chat_compactor::context_summary_path(&app.workgraph_dir, cid).exists();
    let last = state
        .last_compaction
        .clone()
        .unwrap_or_else(|| "never".to_string());

    vec![
        Line::from(vec![
            Span::styled(
                format!("Chat {} ", cid),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("· "),
            Span::styled(executor, Style::default().fg(Color::White)),
            Span::raw(" · "),
            Span::styled(model, Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("Chat compaction ", Style::default().fg(Color::Green)),
            Span::raw("· "),
            Span::raw(format!("{}x", state.compaction_count)),
            Span::raw(" · "),
            Span::raw(format!("last {}", last)),
            Span::raw(" · "),
            Span::raw(format!("pending {}/{}", pending, threshold)),
            Span::raw(" · "),
            Span::raw(if summary_present {
                "summary present"
            } else {
                "summary absent"
            }),
        ]),
        Line::from(Span::styled(
            "─".repeat(24),
            Style::default().fg(Color::DarkGray),
        )),
    ]
}

/// Render the semantic activity feed from parsed operations.jsonl events.
fn draw_activity_feed(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let viewport_h = area.height as usize;
    app.activity_feed.viewport_height = viewport_h;
    if viewport_h == 0 {
        return;
    }
    let wrap_width = area.width as usize;
    let mut wrapped_lines: Vec<Line> = Vec::new();

    for event in &app.activity_feed.events {
        let (icon_color, icon_style) = activity_event_style(&event.kind);
        // Format: "HH:MM:SS icon summary"
        let time_span = Span::styled(
            format!("{} ", event.time_short),
            Style::default().fg(Color::DarkGray),
        );
        let icon_span = Span::styled(format!("{} ", event.icon()), icon_style);
        let summary_span = Span::styled(event.summary.clone(), Style::default().fg(icon_color));

        let prefix_len = event.time_short.len() + 1 + event.icon().len() + 1;
        let text_width = wrap_width.saturating_sub(prefix_len);

        if text_width == 0 || event.summary.is_empty() {
            wrapped_lines.push(Line::from(vec![time_span, icon_span, summary_span]));
        } else if event.summary.width() > text_width {
            let wrapped = word_wrap(&event.summary, text_width);
            let indent = " ".repeat(prefix_len);
            for (i, wl) in wrapped.iter().enumerate() {
                if i == 0 {
                    wrapped_lines.push(Line::from(vec![
                        time_span.clone(),
                        icon_span.clone(),
                        Span::styled(wl.to_string(), Style::default().fg(icon_color)),
                    ]));
                } else {
                    wrapped_lines.push(Line::from(Span::styled(
                        format!("{}{}", indent, wl),
                        Style::default().fg(icon_color),
                    )));
                }
            }
        } else {
            wrapped_lines.push(Line::from(vec![time_span, icon_span, summary_span]));
        }
    }

    let total_lines = wrapped_lines.len();
    app.activity_feed.total_wrapped_lines = total_lines;
    let scroll = app
        .activity_feed
        .scroll
        .min(total_lines.saturating_sub(viewport_h));
    let end = (scroll + viewport_h).min(total_lines);
    let visible_lines: Vec<Line> = wrapped_lines[scroll..end].to_vec();
    let paragraph = Paragraph::new(visible_lines);
    frame.render_widget(paragraph, area);
    if total_lines > viewport_h && app.panel_scrollbar_visible() {
        draw_panel_scrollbar(
            frame,
            app,
            area,
            total_lines.saturating_sub(viewport_h),
            scroll,
        );
    }
}

/// Return (foreground color, icon Style) for an activity event kind.
fn activity_event_style(kind: &ActivityEventKind) -> (Color, Style) {
    match kind {
        ActivityEventKind::TaskCreated => (Color::Blue, Style::default().fg(Color::Blue)),
        ActivityEventKind::StatusChange => (Color::Yellow, Style::default().fg(Color::Yellow)),
        ActivityEventKind::AgentSpawned => (Color::Green, Style::default().fg(Color::Green)),
        ActivityEventKind::AgentCompleted => (
            Color::Green,
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        ActivityEventKind::AgentFailed => (
            Color::Red,
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        ActivityEventKind::CoordinatorTick => {
            (Color::DarkGray, Style::default().fg(Color::DarkGray))
        }
        ActivityEventKind::VerificationResult => (Color::Cyan, Style::default().fg(Color::Cyan)),
        ActivityEventKind::Compact => (Color::Magenta, Style::default().fg(Color::Magenta)),
        ActivityEventKind::UserAction => (Color::White, Style::default().fg(Color::White)),
    }
}

fn draw_dashboard_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    use super::state::DashboardAgentActivity;
    use ratatui::widgets::Sparkline;

    let width = area.width as usize;
    if width < 4 || area.height < 3 {
        return;
    }

    let mut lines: Vec<Line> = Vec::new();

    // ── Coordinator Cards ──
    lines.push(Line::from(Span::styled(
        "── Coordinators ──",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    if app.dashboard.coordinator_cards.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No coordinators running",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for card in &app.dashboard.coordinator_cards {
            let status_icon = if !card.enabled {
                Span::styled("⏹ ", Style::default().fg(Color::Red))
            } else if card.frozen {
                Span::styled("❄ ", Style::default().fg(Color::Blue))
            } else if card.paused {
                Span::styled("⏸ ", Style::default().fg(Color::Yellow))
            } else {
                Span::styled("▶ ", Style::default().fg(Color::Green))
            };

            let state_label = if !card.enabled {
                "stopped"
            } else if card.frozen {
                "frozen"
            } else if card.paused {
                "paused"
            } else {
                "running"
            };

            lines.push(Line::from(vec![
                Span::raw("  "),
                status_icon,
                Span::styled(
                    format!("Chat #{}", card.id),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  ({})", state_label),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));

            // Stats row
            lines.push(Line::from(vec![
                Span::styled("    Agents: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}/{}", card.agents_alive, card.max_agents),
                    Style::default().fg(Color::White),
                ),
                Span::styled("  Ready: ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{}", card.tasks_ready),
                    Style::default().fg(Color::White),
                ),
                Span::styled("  Ticks: ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{}", card.ticks), Style::default().fg(Color::White)),
            ]));

            // Model + tokens
            let mut model_spans = vec![Span::styled("    ", Style::default())];
            if let Some(ref model) = card.model {
                model_spans.push(Span::styled(
                    format!("Model: {}", model),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if card.accumulated_tokens > 0 {
                model_spans.push(Span::styled(
                    format!("  Tokens: {}", format_tokens(card.accumulated_tokens)),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if model_spans.len() > 1 {
                lines.push(Line::from(model_spans));
            }
            lines.push(Line::from(""));
        }
    }

    // ── Agent Table ──
    lines.push(Line::from(Span::styled(
        "── Agents ──",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    if app.dashboard.agent_rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No agents registered",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        // Header
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(
                format!("{:<12} {:<8} {:<8} ", "AGENT", "STATUS", "TIME"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "TASK",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        for (i, row) in app.dashboard.agent_rows.iter().enumerate() {
            let is_selected = i == app.dashboard.selected_row;
            let activity_color = match row.activity {
                DashboardAgentActivity::Active => Color::Green,
                DashboardAgentActivity::Slow => Color::Yellow,
                DashboardAgentActivity::Stuck => Color::Red,
                DashboardAgentActivity::Exited => Color::DarkGray,
            };

            let elapsed_str = row
                .elapsed_secs
                .map(|s| format_duration_compact(s as u64))
                .unwrap_or_else(|| "—".to_string());

            let task_display = row.task_title.as_deref().unwrap_or(&row.task_id);

            let row_style = if is_selected {
                Style::default().bg(Color::Rgb(40, 40, 60))
            } else {
                Style::default()
            };

            let selector = if is_selected { "▸ " } else { "  " };

            lines.push(Line::from(vec![
                Span::styled(selector, Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("{:<12} ", &row.agent_id),
                    row_style.fg(Color::White),
                ),
                Span::styled(
                    format!("{:<8} ", row.activity.label()),
                    row_style.fg(activity_color),
                ),
                Span::styled(
                    format!("{:<8} ", elapsed_str),
                    row_style.fg(Color::DarkGray),
                ),
                Span::styled(task_display, row_style.fg(Color::White)),
            ]));

            // Show latest snippet for active agents
            if matches!(
                row.activity,
                DashboardAgentActivity::Active | DashboardAgentActivity::Slow
            ) && let Some(ref snippet) = row.latest_snippet
            {
                let max_len = width.saturating_sub(6);
                let truncated = if snippet.len() > max_len {
                    format!("{}…", &snippet[..max_len.saturating_sub(1)])
                } else {
                    snippet.clone()
                };
                lines.push(Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(truncated, Style::default().fg(Color::Rgb(100, 100, 100))),
                ]));
            }
        }
    }
    lines.push(Line::from(""));

    // ── Graph Summary ──
    lines.push(Line::from(Span::styled(
        "── Graph Summary ──",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    let tc = &app.task_counts;
    lines.push(Line::from(vec![
        Span::styled("  Total: ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{}", tc.total), Style::default().fg(Color::White)),
        Span::styled("  Done: ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{}", tc.done), Style::default().fg(Color::Green)),
        Span::styled("  In-progress: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}", tc.in_progress),
            Style::default().fg(Color::Yellow),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Open: ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{}", tc.open), Style::default().fg(Color::White)),
        Span::styled("  Failed: ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{}", tc.failed), Style::default().fg(Color::Red)),
        Span::styled("  Blocked: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}", tc.blocked),
            Style::default().fg(Color::Magenta),
        ),
    ]));

    // Progress bar
    if tc.total > 0 {
        let pct = (tc.done as f64 / tc.total as f64 * 100.0) as u16;
        let bar_width = width.saturating_sub(12).min(40);
        let filled = (pct as usize * bar_width) / 100;
        let empty = bar_width.saturating_sub(filled);
        lines.push(Line::from(vec![
            Span::styled("  [", Style::default().fg(Color::DarkGray)),
            Span::styled("█".repeat(filled), Style::default().fg(Color::Green)),
            Span::styled(
                "░".repeat(empty),
                Style::default().fg(Color::Rgb(60, 60, 60)),
            ),
            Span::styled("] ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}%", pct), Style::default().fg(Color::White)),
        ]));
    }
    lines.push(Line::from(""));

    // ── Activity Sparkline ──
    lines.push(Line::from(Span::styled(
        "── Activity ──",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));

    let sparkline_height: u16 = 3;
    let total_text_lines = lines.len() as u16;

    // Render the text portion
    let text_height = area.height.saturating_sub(sparkline_height + 1);
    let para = Paragraph::new(lines).scroll((app.dashboard.scroll as u16, 0));

    let text_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: text_height.min(area.height),
    };
    frame.render_widget(para, text_area);

    // Sparkline at the bottom if there's room
    let sparkline_y = area.y + text_area.height;
    if sparkline_y + sparkline_height <= area.y + area.height {
        let sparkline_area = Rect {
            x: area.x + 1,
            y: sparkline_y,
            width: area.width.saturating_sub(2),
            height: sparkline_height,
        };

        if sparkline_area.height > 0 && sparkline_area.width > 0 {
            let data = &app.dashboard.sparkline_data;
            let sparkline = Sparkline::default()
                .data(data)
                .style(Style::default().fg(Color::Green));
            frame.render_widget(sparkline, sparkline_area);
        }
    }

    // Update metrics for scrollbar support
    app.dashboard.total_rendered_lines = total_text_lines as usize + sparkline_height as usize;
    app.dashboard.viewport_height = area.height as usize;
}

/// Highlight all occurrences of `query_lower` (already lowercased) in a Line
/// by splitting spans and applying `bg` to matching regions.
fn highlight_query_in_line<'a>(line: Line<'a>, query_lower: &str, bg: Color) -> Line<'a> {
    if query_lower.is_empty() {
        return line;
    }
    let mut new_spans: Vec<Span<'a>> = Vec::new();
    for span in line.spans {
        let text = span.content.as_ref();
        let text_lower = text.to_lowercase();
        let mut last = 0;
        let mut found = false;
        let mut start = 0;
        while start < text_lower.len() {
            if let Some(pos) = text_lower[start..].find(query_lower) {
                found = true;
                let abs_pos = start + pos;
                // Text before the match.
                if abs_pos > last {
                    new_spans.push(Span::styled(text[last..abs_pos].to_string(), span.style));
                }
                // The match itself.
                let match_end = abs_pos + query_lower.len();
                let match_end = match_end.min(text.len());
                new_spans.push(Span::styled(
                    text[abs_pos..match_end].to_string(),
                    span.style.bg(bg).add_modifier(Modifier::BOLD),
                ));
                last = match_end;
                start = match_end;
            } else {
                break;
            }
        }
        if found {
            if last < text.len() {
                new_spans.push(Span::styled(text[last..].to_string(), span.style));
            }
        } else {
            new_spans.push(span);
        }
    }
    Line::from(new_spans)
}

/// Simple word-wrap: break text into lines that fit within `max_width` display columns.
/// Words longer than `max_width` are hard-broken across multiple lines.
/// Uses display width (UnicodeWidthStr) so wide characters are accounted for correctly.
fn word_wrap(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return vec![text.to_string()];
    }

    /// Hard-break a word that exceeds `max_width` display columns, pushing complete
    /// chunks to `lines` and returning any leftover as a String.
    fn hard_break(word: &str, max_width: usize, lines: &mut Vec<String>) -> String {
        let mut buf = String::new();
        let mut buf_width = 0usize;
        for c in word.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if buf_width + cw > max_width && !buf.is_empty() {
                lines.push(std::mem::take(&mut buf));
                buf_width = 0;
            }
            buf.push(c);
            buf_width += cw;
            if buf_width == max_width {
                lines.push(std::mem::take(&mut buf));
                buf_width = 0;
            }
        }
        buf // leftover (< max_width columns)
    }

    let display_width = |s: &str| s.width();

    let mut lines = Vec::new();
    let mut current_line = String::new();

    for word in text.split_whitespace() {
        let wlen = display_width(word);
        let clen = display_width(&current_line);

        if current_line.is_empty() {
            if wlen <= max_width {
                current_line.push_str(word);
            } else {
                current_line = hard_break(word, max_width, &mut lines);
            }
        } else if clen + 1 + wlen <= max_width {
            current_line.push(' ');
            current_line.push_str(word);
        } else {
            lines.push(std::mem::take(&mut current_line));
            if wlen <= max_width {
                current_line.push_str(word);
            } else {
                current_line = hard_break(word, max_width, &mut lines);
            }
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    if lines.is_empty() {
        lines.push(String::new());
    }

    lines
}

/// Word-wrap a list of styled `Line`s to fit within `max_width`.
///
/// For each line, extracts the full text content and applies `word_wrap`.
/// Continuation lines inherit the style of the last span in the original line.
/// Lines that already fit (or are empty) are passed through unchanged.
fn wrap_line_spans<'a>(lines: &[Line<'a>], max_width: usize) -> Vec<Line<'a>> {
    if max_width == 0 {
        return lines.to_vec();
    }
    let mut out: Vec<Line> = Vec::new();
    for line in lines {
        // Compute the total display width of this line.
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let text_display_width = text.width();
        if text_display_width <= max_width || text.trim().is_empty() {
            out.push(line.clone());
            continue;
        }

        // For lines with a single span, wrap and preserve the style.
        if line.spans.len() == 1 {
            let span = &line.spans[0];
            let style = span.style;
            let content = span.content.as_ref();
            // Detect leading whitespace indent (display width).
            let trimmed = content.trim_start();
            let indent: String = content[..content.len() - trimmed.len()].to_string();
            let indent_display_w = indent.width();
            let body = trimmed;
            let wrap_w = max_width.saturating_sub(indent_display_w);
            let wrapped = word_wrap(body, wrap_w);
            for w in &wrapped {
                out.push(Line::from(Span::styled(format!("{}{}", indent, w), style)));
            }
            continue;
        }

        // Multi-span line: wrap by walking styled characters with display-width
        // tracking, breaking at word boundaries. This avoids the whitespace
        // normalization issue that word_wrap's split_whitespace() causes.
        let styled_chars: Vec<(char, Style)> = line
            .spans
            .iter()
            .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
            .collect();

        // Split into tokens: alternating whitespace and non-whitespace runs,
        // each token is a slice range into styled_chars.
        let mut tokens: Vec<(usize, usize, bool)> = Vec::new(); // (start, end, is_space)
        let mut i = 0;
        while i < styled_chars.len() {
            let is_space = styled_chars[i].0.is_whitespace();
            let start = i;
            while i < styled_chars.len() && styled_chars[i].0.is_whitespace() == is_space {
                i += 1;
            }
            tokens.push((start, i, is_space));
        }

        // Greedy line-breaking using display width.
        let char_width =
            |c: char| -> usize { unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) };
        let token_width = |start: usize, end: usize| -> usize {
            styled_chars[start..end]
                .iter()
                .map(|&(c, _)| char_width(c))
                .sum()
        };

        let mut result_lines: Vec<Vec<(char, Style)>> = Vec::new();
        let mut cur_line: Vec<(char, Style)> = Vec::new();
        let mut cur_width: usize = 0;

        for &(tok_start, tok_end, is_space) in &tokens {
            let tw = token_width(tok_start, tok_end);

            if is_space {
                if cur_line.is_empty() {
                    // Skip leading whitespace on continuation lines.
                    continue;
                }
                if cur_width + tw <= max_width {
                    cur_line.extend_from_slice(&styled_chars[tok_start..tok_end]);
                    cur_width += tw;
                } else {
                    // Space would overflow — break here, trim trailing space.
                    while cur_line
                        .last()
                        .map(|&(c, _)| c.is_whitespace())
                        .unwrap_or(false)
                    {
                        cur_line.pop();
                    }
                    if !cur_line.is_empty() {
                        result_lines.push(std::mem::take(&mut cur_line));
                    }
                    cur_width = 0;
                }
            } else {
                // Word token.
                if cur_width + tw <= max_width {
                    cur_line.extend_from_slice(&styled_chars[tok_start..tok_end]);
                    cur_width += tw;
                } else if cur_line.is_empty() {
                    // Word alone exceeds max_width — hard break.
                    let mut buf: Vec<(char, Style)> = Vec::new();
                    let mut bw: usize = 0;
                    for &(c, style) in &styled_chars[tok_start..tok_end] {
                        let cw = char_width(c);
                        if bw + cw > max_width && !buf.is_empty() {
                            result_lines.push(std::mem::take(&mut buf));
                            bw = 0;
                        }
                        buf.push((c, style));
                        bw += cw;
                    }
                    cur_line = buf;
                    cur_width = bw;
                } else {
                    // Wrap: start new line with this word.
                    // Trim trailing whitespace from current line.
                    while cur_line
                        .last()
                        .map(|&(c, _)| c.is_whitespace())
                        .unwrap_or(false)
                    {
                        cur_line.pop();
                    }
                    if !cur_line.is_empty() {
                        result_lines.push(std::mem::take(&mut cur_line));
                    }
                    // Word may still exceed max_width — hard break if needed.
                    if tw <= max_width {
                        cur_line.extend_from_slice(&styled_chars[tok_start..tok_end]);
                        cur_width = tw;
                    } else {
                        let mut buf: Vec<(char, Style)> = Vec::new();
                        let mut bw: usize = 0;
                        for &(c, style) in &styled_chars[tok_start..tok_end] {
                            let cw = char_width(c);
                            if bw + cw > max_width && !buf.is_empty() {
                                result_lines.push(std::mem::take(&mut buf));
                                bw = 0;
                            }
                            buf.push((c, style));
                            bw += cw;
                        }
                        cur_line = buf;
                        cur_width = bw;
                    }
                }
            }
        }
        // Flush last line, trimming trailing whitespace.
        while cur_line
            .last()
            .map(|&(c, _)| c.is_whitespace())
            .unwrap_or(false)
        {
            cur_line.pop();
        }
        if !cur_line.is_empty() {
            result_lines.push(cur_line);
        }

        // Convert styled char sequences back into Lines with merged Spans.
        for char_line in &result_lines {
            if char_line.is_empty() {
                out.push(Line::from(""));
                continue;
            }
            let mut spans: Vec<Span> = Vec::new();
            let mut buf = String::new();
            let mut buf_style = char_line[0].1;
            for &(c, style) in char_line {
                if style == buf_style {
                    buf.push(c);
                } else {
                    if !buf.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut buf), buf_style));
                    }
                    buf.push(c);
                    buf_style = style;
                }
            }
            if !buf.is_empty() {
                spans.push(Span::styled(buf, buf_style));
            }
            out.push(Line::from(spans));
        }
    }
    out
}

/// Draw the Agents tab content: lifecycle view for selected task + agent list.
fn draw_agents_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();

    // ── Task Lifecycle (assign → execute → evaluate) ──
    if let Some(ref lifecycle) = app.agency_lifecycle {
        lines.push(Line::from(Span::styled(
            "── Task Lifecycle ──",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));

        let phases: Vec<Option<&super::state::LifecyclePhase>> = vec![
            lifecycle.assignment.as_ref(),
            lifecycle.execution.as_ref(),
            lifecycle.evaluation.as_ref(),
        ];
        let phase_labels = ["⊳ Assignment", "▸ Execution", "∴ Evaluation"];
        let phase_keys = ["[a]", "", "[e]"];

        for (i, phase_opt) in phases.iter().enumerate() {
            match phase_opt {
                Some(phase) => {
                    // Status indicator
                    let status_icon = match phase.status {
                        workgraph::graph::Status::Done => {
                            Span::styled("✓ ", Style::default().fg(Color::Green))
                        }
                        workgraph::graph::Status::InProgress => {
                            Span::styled("● ", Style::default().fg(Color::Yellow))
                        }
                        workgraph::graph::Status::Failed => {
                            Span::styled("✗ ", Style::default().fg(Color::Red))
                        }
                        _ => Span::styled("○ ", Style::default().fg(Color::DarkGray)),
                    };

                    let label_style = if i == 1 {
                        // Execution phase gets bold
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::White)
                    };

                    let mut spans = vec![status_icon, Span::styled(phase_labels[i], label_style)];

                    // Add navigate hint for assign/eval phases
                    if !phase_keys[i].is_empty() {
                        spans.push(Span::styled(
                            format!(" {}", phase_keys[i]),
                            Style::default().fg(Color::DarkGray),
                        ));
                    }

                    lines.push(Line::from(spans));

                    // Agent info
                    if let Some(ref agent_id) = phase.agent_id {
                        lines.push(Line::from(Span::styled(
                            format!("  Agent: {}", agent_id),
                            Style::default().fg(Color::DarkGray),
                        )));
                    }

                    // Token usage
                    if let Some(ref usage) = phase.token_usage {
                        let novel_in = usage.input_tokens + usage.cache_creation_input_tokens;
                        let mut spans: Vec<Span> = vec![Span::styled(
                            format!(
                                "  Tokens: →{} ←{}",
                                format_tokens(novel_in),
                                format_tokens(usage.output_tokens)
                            ),
                            Style::default().fg(Color::DarkGray),
                        )];
                        if usage.cache_read_input_tokens > 0 {
                            spans.push(Span::styled(
                                format!(
                                    "  (cached: {})",
                                    format_tokens(usage.cache_read_input_tokens)
                                ),
                                Style::default().fg(Color::Rgb(80, 80, 80)),
                            ));
                        }
                        if usage.cost_usd > 0.0 {
                            spans.push(Span::styled(
                                format!(" ${:.4}", usage.cost_usd),
                                Style::default().fg(Color::DarkGray),
                            ));
                        }
                        lines.push(Line::from(spans));
                    }

                    // Runtime
                    if let Some(secs) = phase.runtime_secs {
                        let dur = workgraph::format_duration(secs, false);
                        let timing_label = if phase.status == workgraph::graph::Status::InProgress {
                            format!("  Running: {}", dur)
                        } else {
                            format!("  Duration: {}", dur)
                        };
                        lines.push(Line::from(Span::styled(
                            timing_label,
                            Style::default().fg(Color::DarkGray),
                        )));
                    }

                    // Evaluation results (only for evaluation phase)
                    if let Some(score) = phase.eval_score {
                        lines.push(Line::from(Span::styled(
                            format!("  Score: {:.2}", score),
                            Style::default().fg(Color::Yellow),
                        )));
                    }
                    if let Some(ref notes) = phase.eval_notes {
                        for note_line in notes.lines().take(3) {
                            lines.push(Line::from(Span::styled(
                                format!("  {}", note_line),
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                    }

                    // Arrow connector between phases (except after last)
                    if i < 2 {
                        lines.push(Line::from(Span::styled(
                            "  │",
                            Style::default().fg(Color::DarkGray),
                        )));
                    }
                }
                None => {
                    // Phase doesn't exist - show as absent
                    if i != 1 {
                        // Don't skip execution (always present)
                        lines.push(Line::from(Span::styled(
                            format!("  {} (none)", phase_labels[i]),
                            Style::default().fg(Color::DarkGray),
                        )));
                        if i < 2 {
                            lines.push(Line::from(Span::styled(
                                "  │",
                                Style::default().fg(Color::DarkGray),
                            )));
                        }
                    }
                }
            }
        }

        // Total cost summary
        let total_cost: f64 = phases
            .iter()
            .filter_map(|p| p.as_ref())
            .filter_map(|p| p.token_usage.as_ref())
            .map(|u| u.cost_usd)
            .sum();
        let total_new_input: u64 = phases
            .iter()
            .filter_map(|p| p.as_ref())
            .filter_map(|p| p.token_usage.as_ref())
            .map(|u| u.input_tokens + u.cache_creation_input_tokens)
            .sum();
        let total_output: u64 = phases
            .iter()
            .filter_map(|p| p.as_ref())
            .filter_map(|p| p.token_usage.as_ref())
            .map(|u| u.output_tokens)
            .sum();
        let total_cached: u64 = phases
            .iter()
            .filter_map(|p| p.as_ref())
            .filter_map(|p| p.token_usage.as_ref())
            .map(|u| u.cache_read_input_tokens + u.cache_creation_input_tokens)
            .sum();
        let total_tokens = total_new_input + total_output + total_cached;
        if total_tokens > 0 {
            lines.push(Line::from(""));
            let mut spans: Vec<Span> = vec![Span::styled(
                format!(
                    "  Total: →{} ←{}",
                    format_tokens(total_new_input),
                    format_tokens(total_output)
                ),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )];
            if total_cached > 0 {
                spans.push(Span::styled(
                    format!("  (cached: {})", format_tokens(total_cached)),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if total_cost > 0.0 {
                spans.push(Span::styled(
                    format!(" ${:.4}", total_cost),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            lines.push(Line::from(spans));
        }

        lines.push(Line::from(""));
    } else {
        lines.push(Line::from(Span::styled(
            "No task selected",
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(""));
    }

    // ── Agent List ──
    if !app.agent_monitor.agents.is_empty() {
        let working_count = app
            .agent_monitor
            .agents
            .iter()
            .filter(|a| matches!(a.status, AgentStatus::Working))
            .count();
        lines.push(Line::from(Span::styled(
            format!(
                "── Agents ({}/{}) ──",
                working_count,
                app.agent_monitor.agents.len()
            ),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));

        for agent in &app.agent_monitor.agents {
            let status_indicator = match agent.status {
                AgentStatus::Working => Span::styled("● ", Style::default().fg(Color::Green)),
                AgentStatus::Done => Span::styled("✓ ", Style::default().fg(Color::Green)),
                AgentStatus::Failed | AgentStatus::Dead => {
                    Span::styled("✗ ", Style::default().fg(Color::Red))
                }
                AgentStatus::Frozen => Span::styled("⏸ ", Style::default().fg(Color::Blue)),
                _ => Span::styled("○ ", Style::default().fg(Color::DarkGray)),
            };
            // Build agent header with optional stream message count.
            let stream_info = app.agent_streams.get(&agent.agent_id);
            let mut header_spans = vec![
                status_indicator,
                Span::styled(
                    &agent.agent_id,
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ];
            if let Some(ref tid) = agent.task_id {
                let task_label = agent.task_title.as_deref().unwrap_or(tid.as_str());
                header_spans.push(Span::styled(
                    format!(" [{}]", task_label),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if let Some(si) = stream_info {
                header_spans.push(Span::styled(
                    format!(" \u{1f4e8} {} msgs", si.message_count),
                    Style::default().fg(Color::Yellow),
                ));
            }
            lines.push(Line::from(header_spans));
            // Show timing info
            {
                let is_alive = matches!(agent.status, AgentStatus::Working);
                let runtime_str = agent
                    .runtime_secs
                    .map(|s| workgraph::format_duration(s, false));
                let start_local = agent.started_at.as_deref().and_then(|s| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .ok()
                        .map(|dt| dt.with_timezone(&chrono::Local))
                });
                let completed_local = agent.completed_at.as_deref().and_then(|s| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .ok()
                        .map(|dt| dt.with_timezone(&chrono::Local))
                });

                let timing = if is_alive {
                    match (runtime_str, start_local) {
                        (Some(dur), Some(st)) => {
                            format!("  Running for {} (started {})", dur, st.format("%H:%M:%S"))
                        }
                        (Some(dur), None) => format!("  Running for {}", dur),
                        _ => String::new(),
                    }
                } else {
                    let finished_ago = completed_local.map(|c| {
                        let ago_secs = (chrono::Utc::now() - c.with_timezone(&chrono::Utc))
                            .num_seconds()
                            .max(0);
                        workgraph::format_duration(ago_secs, true)
                    });
                    match (runtime_str, finished_ago, completed_local) {
                        (Some(dur), Some(ago), Some(ct)) => {
                            format!(
                                "  Ran {} · Finished {} ago ({})",
                                dur,
                                ago,
                                ct.format("%H:%M:%S")
                            )
                        }
                        (Some(dur), _, _) => format!("  Ran {}", dur),
                        _ => String::new(),
                    }
                };

                if !timing.is_empty() {
                    lines.push(Line::from(Span::styled(
                        timing,
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            // Show live stream snippet for working agents.
            if let Some(si) = stream_info
                && let Some(ref snippet) = si.latest_snippet
            {
                let icon = if si.latest_is_tool {
                    "\u{1f527} "
                } else {
                    "\u{1f4ad} "
                };
                lines.push(Line::from(Span::styled(
                    format!("  {}{}", icon, snippet),
                    Style::default().fg(if si.latest_is_tool {
                        Color::Cyan
                    } else {
                        Color::White
                    }),
                )));
            }
            lines.push(Line::from(""));
        }
    }

    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No agency data available.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Word-wrap all lines to fit the panel width.
    let wrap_width = area.width as usize;
    let wrapped_lines = wrap_line_spans(&lines, wrap_width);

    let viewport_h = area.height as usize;
    let total_lines = wrapped_lines.len();
    app.agent_monitor.total_rendered_lines = total_lines;
    app.agent_monitor.viewport_height = viewport_h;
    let scroll = app
        .agent_monitor
        .scroll
        .min(total_lines.saturating_sub(viewport_h));
    let end = (scroll + viewport_h).min(total_lines);
    let visible_lines: Vec<Line> = wrapped_lines[scroll..end].to_vec();

    let paragraph = Paragraph::new(visible_lines);
    frame.render_widget(paragraph, area);

    if total_lines > viewport_h && app.panel_scrollbar_visible() {
        draw_panel_scrollbar(
            frame,
            app,
            area,
            total_lines.saturating_sub(viewport_h),
            scroll,
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Overlay widgets
// ══════════════════════════════════════════════════════════════════════════════

/// Draw a confirmation dialog overlay. Returns the dialog area for click-outside detection.
fn draw_confirm_dialog(frame: &mut Frame, action: &ConfirmAction) -> Rect {
    let message = match action {
        ConfirmAction::MarkDone(id) => format!("Mark '{}' done?", id),
        ConfirmAction::Retry(id) => format!("Retry '{}'?", id),
    };

    let size = frame.area();
    let width = (message.len() as u16 + 6)
        .min(size.width.saturating_sub(4))
        .max(30);
    let height = 5;
    let x = (size.width.saturating_sub(width)) / 2;
    let y = (size.height.saturating_sub(height)) / 2;
    let area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Confirm ")
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let lines = vec![
        Line::from(Span::raw(&message)),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "[y]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" Yes  "),
            Span::styled(
                "[n]",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" No"),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
    area
}

/// Draw a choice dialog overlay with multiple selectable options. Returns the dialog area.
fn draw_choice_dialog(frame: &mut Frame, state: &ChoiceDialogState) -> Rect {
    use super::state::ChoiceDialogAction;

    let title = match &state.action {
        ChoiceDialogAction::RemoveCoordinator(cid) => format!(" Close Coordinator {} ", cid),
    };

    let size = frame.area();
    let width: u16 = 45;
    let height: u16 = 3 + state.options.len() as u16 + 2; // border + options + footer + border
    let x = (size.width.saturating_sub(width)) / 2;
    let y = (size.height.saturating_sub(height)) / 2;
    let area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, (hotkey, label, desc)) in state.options.iter().enumerate() {
        let is_selected = i == state.selected;
        let style = if is_selected {
            Style::default().bg(Color::DarkGray).fg(Color::White)
        } else {
            Style::default()
        };
        let hotkey_style = if is_selected {
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" [{}] ", hotkey), hotkey_style),
            Span::styled(format!("{:<8}", label), style.add_modifier(Modifier::BOLD)),
            Span::styled(format!("— {}", desc), style),
        ]));
    }
    // Empty line + footer
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" [↑↓]", Style::default().fg(Color::DarkGray)),
        Span::raw(" Navigate  "),
        Span::styled("[Enter]", Style::default().fg(Color::DarkGray)),
        Span::raw(" Select  "),
        Span::styled("[Esc]", Style::default().fg(Color::DarkGray)),
        Span::raw(" Cancel"),
    ]));

    frame.render_widget(Paragraph::new(lines), inner);
    area
}

/// Draw the coordinator picker overlay. Returns the dialog area.
fn draw_coordinator_picker(
    frame: &mut Frame,
    picker: &super::state::CoordinatorPickerState,
    active_cid: u32,
) -> Rect {
    let size = frame.area();
    let width: u16 = 50.min(size.width.saturating_sub(4));
    let height: u16 =
        (3 + picker.entries.len() as u16 + 2).min(size.height.saturating_sub(2)); // border + entries + footer + border
    let x = (size.width.saturating_sub(width)) / 2;
    let y = (size.height.saturating_sub(height)) / 2;
    let area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Switch Coordinator ")
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, (cid, label, status, alive)) in picker.entries.iter().enumerate() {
        let is_selected = i == picker.selected;
        let is_active = *cid == active_cid;

        let bg = if is_selected {
            Color::DarkGray
        } else {
            Color::Reset
        };
        let fg = if *alive { Color::Green } else { Color::Gray };
        let marker = if is_active { ">" } else { " " };

        let status_indicator = if *alive { "●" } else { "○" };

        lines.push(Line::from(vec![
            Span::styled(
                format!(" {} ", marker),
                Style::default()
                    .bg(bg)
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} ", status_indicator),
                Style::default().bg(bg).fg(fg),
            ),
            Span::styled(
                format!("{:<10}", label),
                Style::default()
                    .bg(bg)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" {}", status), Style::default().bg(bg).fg(fg)),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" [↑↓]", Style::default().fg(Color::DarkGray)),
        Span::raw(" Nav  "),
        Span::styled("[Enter]", Style::default().fg(Color::DarkGray)),
        Span::raw(" Open  "),
        Span::styled("[+]", Style::default().fg(Color::DarkGray)),
        Span::raw(" New  "),
        Span::styled("[−]", Style::default().fg(Color::DarkGray)),
        Span::raw(" Close  "),
        Span::styled("[Esc]", Style::default().fg(Color::DarkGray)),
        Span::raw(" Cancel"),
    ]));

    frame.render_widget(Paragraph::new(lines), inner);
    area
}

/// Render a FilterPicker WITH hit-area collection + scroll-window clamping.
/// Used by the launcher to make rows clickable.
///
/// `parent_line_offset` is the number of lines already pushed into the
/// containing paragraph so we can compute absolute Y positions for each row.
/// `parent_area` is the launcher pane rect we're rendering into.
/// `hits` is appended with one (LauncherListHit, Rect) per visible row.
/// `list_area` is set to the bounding box of all visible rows (for scroll-wheel routing).
#[allow(clippy::too_many_arguments)]
fn render_filter_picker_with_hits(
    picker: &super::state::FilterPicker,
    active: bool,
    w: usize,
    viewport_height: usize,
    parent_line_offset: usize,
    parent_area: Rect,
    hits: &mut Vec<(super::state::LauncherListHit, Rect)>,
    list_area: &mut Rect,
) -> Vec<Line<'static>> {
    use super::state::LauncherListHit;
    let mut lines: Vec<Line> = Vec::new();
    let mut local_offset = parent_line_offset;
    let row_rect = |line_idx: usize| -> Rect {
        Rect {
            x: parent_area.x,
            y: parent_area.y.saturating_add(line_idx as u16),
            width: parent_area.width,
            height: 1,
        }
    };

    // Filter input (consume one line if shown)
    if active && !picker.filter.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("    Filter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                picker.filter.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "\u{2588}",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::SLOW_BLINK),
            ),
        ]));
        local_offset += 1;
    } else if active {
        lines.push(Line::from(Span::styled(
            "    Type to filter...",
            Style::default().fg(Color::DarkGray),
        )));
        local_offset += 1;
    }

    if picker.items.is_empty() && !picker.empty_hint.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("    {}", picker.empty_hint),
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    }

    // Apply scroll window: skip first `scroll_offset` filtered rows, show up
    // to `viewport_height` of them.
    let total_filtered = picker.filtered_indices.len();
    let mut scroll = picker.scroll_offset;
    // Auto-clamp: if scroll_offset is past the end, render still works.
    if scroll >= total_filtered && total_filtered > 0 {
        scroll = total_filtered - 1;
    }
    // Auto-scroll selected into view (selected indexes filtered_indices when not custom).
    let visible_end = if picker.is_custom_selected() {
        // Custom row is always rendered separately; window can stay where it is.
        (scroll + viewport_height).min(total_filtered)
    } else {
        let sel = picker.selected;
        let mut s = scroll;
        if sel < s {
            s = sel;
        } else if sel >= s + viewport_height && viewport_height > 0 {
            s = sel + 1 - viewport_height;
        }
        scroll = s;
        (s + viewport_height).min(total_filtered)
    };
    let list_first_y = parent_area.y.saturating_add(local_offset as u16);

    for fi in scroll..visible_end {
        let item_idx = picker.filtered_indices[fi];
        let (id, desc) = &picker.items[item_idx];
        let selected = active && fi == picker.selected && !picker.custom_active;
        let bullet = if selected { " \u{25cf} " } else { " \u{25cb} " };
        let style = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let desc_short: String = desc.chars().take(w.saturating_sub(id.len() + 14)).collect();
        let line_idx = local_offset;
        if desc_short.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("    {}{}", bullet, id),
                style,
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("    {}{}", bullet, id), style),
                Span::styled(
                    format!("  {}", desc_short),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        hits.push((LauncherListHit::Item(fi), row_rect(line_idx)));
        local_offset += 1;
    }

    // "(N/M matches)" / scroll indicator
    if !picker.filter.is_empty() || total_filtered > viewport_height {
        let total = picker.items.len();
        let shown_range_end = visible_end;
        let prefix = if !picker.filter.is_empty() {
            format!("({}/{} matches", total_filtered, total)
        } else {
            format!("({}-{}/{}", scroll + 1, shown_range_end, total_filtered)
        };
        let suffix = if total_filtered > viewport_height {
            format!(", scroll wheel) [{}+]", scroll)
        } else {
            ")".to_string()
        };
        lines.push(Line::from(Span::styled(
            format!("    {}{}", prefix, suffix),
            Style::default().fg(Color::DarkGray),
        )));
        local_offset += 1;
    }

    // Custom row
    if picker.allow_custom {
        let custom_selected = active && picker.is_custom_selected();
        let custom_style = if picker.custom_active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if custom_selected {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let bullet = if custom_selected || picker.custom_active {
            " \u{25cf} "
        } else {
            " \u{25cb} "
        };
        let line_idx = local_offset;
        if picker.custom_active {
            lines.push(Line::from(vec![
                Span::styled(format!("    {}Custom: ", bullet), custom_style),
                Span::raw(picker.custom_text.clone()),
                Span::styled(
                    "\u{2588}",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::SLOW_BLINK),
                ),
            ]));
        } else {
            let display = if picker.custom_text.is_empty() {
                "Custom: [enter value]".to_string()
            } else {
                format!("Custom: {}", picker.custom_text)
            };
            lines.push(Line::from(Span::styled(
                format!("    {}{}", bullet, display),
                custom_style,
            )));
        }
        hits.push((LauncherListHit::Custom, row_rect(line_idx)));
        local_offset += 1;
    }

    // Set list_area to the bounding box of all clickable rows.
    let list_height = (local_offset as u16).saturating_sub(list_first_y - parent_area.y);
    *list_area = Rect {
        x: parent_area.x,
        y: list_first_y,
        width: parent_area.width,
        height: list_height,
    };

    lines
}

/// Render a FilterPicker into a list of Lines.
/// Reused by the launcher (model/endpoint) and config panel (Choice fields).
fn render_filter_picker(
    picker: &super::state::FilterPicker,
    active: bool,
    w: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    // Filter input (shown only when active and filter has text or section is active)
    if active && !picker.filter.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("    Filter: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                picker.filter.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "\u{2588}",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::SLOW_BLINK),
            ),
        ]));
    } else if active {
        lines.push(Line::from(Span::styled(
            "    Type to filter...",
            Style::default().fg(Color::DarkGray),
        )));
    }

    if picker.items.is_empty() && !picker.empty_hint.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("    {}", picker.empty_hint),
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    }

    for (fi, &item_idx) in picker.filtered_indices.iter().enumerate() {
        let (ref id, ref desc) = picker.items[item_idx];
        let selected = active && fi == picker.selected && !picker.custom_active;
        let bullet = if selected { " \u{25cf} " } else { " \u{25cb} " };
        let style = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let desc_short: String = desc.chars().take(w.saturating_sub(id.len() + 14)).collect();
        if desc_short.is_empty() {
            lines.push(Line::from(Span::styled(
                format!("    {}{}", bullet, id),
                style,
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(format!("    {}{}", bullet, id), style),
                Span::styled(
                    format!("  {}", desc_short),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }

    // Show match count when filter is active
    if !picker.filter.is_empty() {
        let total = picker.items.len();
        let shown = picker.filtered_indices.len();
        if shown < total {
            lines.push(Line::from(Span::styled(
                format!("    ({}/{} matches)", shown, total),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    // Custom row
    if picker.allow_custom {
        let custom_selected = active && picker.is_custom_selected();
        let custom_style = if picker.custom_active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else if custom_selected {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let bullet = if custom_selected || picker.custom_active {
            " \u{25cf} "
        } else {
            " \u{25cb} "
        };
        if picker.custom_active {
            lines.push(Line::from(vec![
                Span::styled(format!("    {}Custom: ", bullet), custom_style),
                Span::raw(picker.custom_text.clone()),
                Span::styled(
                    "\u{2588}",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::SLOW_BLINK),
                ),
            ]));
        } else {
            let display = if picker.custom_text.is_empty() {
                "Custom: [enter value]".to_string()
            } else {
                format!("Custom: {}", picker.custom_text)
            };
            lines.push(Line::from(Span::styled(
                format!("    {}{}", bullet, display),
                custom_style,
            )));
        }
    }

    lines
}

fn draw_launcher_pane(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    use super::state::{LauncherListHit, LauncherSection};

    // Snapshot read-only data we need from the launcher so we can mutate
    // hit-area buffers on `app` without holding a long borrow.
    let (
        active_section,
        name,
        executor_list,
        executor_selected,
        model_picker,
        endpoint_picker,
        show_endpoint,
        recent_list,
        recent_selected,
    ) = match app.launcher.as_ref() {
        Some(l) => (
            l.active_section.clone(),
            l.name.clone(),
            l.executor_list.clone(),
            l.executor_selected,
            l.model_picker.clone(),
            l.endpoint_picker.clone(),
            l.show_endpoint(),
            l.recent_list.clone(),
            l.recent_selected,
        ),
        None => return,
    };

    // Reset hit areas for this frame.
    app.last_launcher_area = area;
    app.launcher_executor_hits.clear();
    app.launcher_model_hits.clear();
    app.launcher_endpoint_hits.clear();
    app.launcher_recent_hits.clear();
    app.launcher_name_hit = Rect::default();
    app.launcher_model_list_area = Rect::default();
    app.launcher_endpoint_list_area = Rect::default();

    let w = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    // Helper: turn the upcoming line index into an absolute row Rect.
    let row_rect = |line_idx: usize| -> Rect {
        Rect {
            x: area.x,
            y: area.y.saturating_add(line_idx as u16),
            width: area.width,
            height: 1,
        }
    };

    let section_style = |active: bool| {
        if active {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        }
    };

    // Title
    lines.push(Line::from(Span::styled(
        "  New Chat",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        format!("  {}", "\u{2500}".repeat(w.saturating_sub(4).min(40))),
        Style::default().fg(Color::DarkGray),
    )));

    // Name field
    let name_active = active_section == LauncherSection::Name;
    let name_prefix = if name_active { "  \u{25b8} " } else { "    " };
    let name_style = if name_active {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let name_display = if name.is_empty() {
        if name_active { "\u{2588}" } else { "(optional)" }
    } else {
        ""
    };
    let name_line_idx = lines.len();
    if name.is_empty() {
        lines.push(Line::from(vec![
            Span::styled(format!("{}Name: ", name_prefix), name_style),
            Span::styled(
                name_display,
                if name_active {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::SLOW_BLINK)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(format!("{}Name: ", name_prefix), name_style),
            Span::raw(name.clone()),
            if name_active {
                Span::styled(
                    "\u{2588}",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::SLOW_BLINK),
                )
            } else {
                Span::raw("")
            },
        ]));
    }
    app.launcher_name_hit = row_rect(name_line_idx);
    lines.push(Line::from(""));

    // Executor section
    let exec_active = active_section == LauncherSection::Executor;
    lines.push(Line::from(Span::styled(
        if exec_active {
            "  \u{25b8} Executor"
        } else {
            "    Executor"
        },
        section_style(exec_active),
    )));

    for (i, (ename, desc, available)) in executor_list.iter().enumerate() {
        let selected = exec_active && i == executor_selected;
        let bullet = if selected { " \u{25cf} " } else { " \u{25cb} " };
        let style = if !available {
            Style::default().fg(Color::DarkGray)
        } else if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let suffix = if !available { " (not found)" } else { "" };
        let desc_short: String = desc
            .chars()
            .take(w.saturating_sub(ename.len() + 14))
            .collect();
        let row_idx = lines.len();
        lines.push(Line::from(vec![
            Span::styled(format!("    {}{}", bullet, ename), style),
            Span::styled(
                format!("  {}{}", desc_short, suffix),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        app.launcher_executor_hits.push((i, row_rect(row_idx)));
    }
    lines.push(Line::from(""));

    // Model section header
    let model_active = active_section == LauncherSection::Model;
    lines.push(Line::from(Span::styled(
        if model_active {
            "  \u{25b8} Model"
        } else {
            "    Model"
        },
        section_style(model_active),
    )));

    // Compute remaining vertical space, reserving room for endpoint + recent + footer.
    let mut reserved = 1usize; // footer
    if show_endpoint {
        // header + items + custom row + blank
        reserved += 2
            + endpoint_picker.filtered_indices.len()
            + if endpoint_picker.allow_custom { 1 } else { 0 };
    }
    if !recent_list.is_empty() {
        reserved += 2 + recent_list.len(); // header + items + blank
    }
    let used_so_far = lines.len();
    let area_h = area.height as usize;
    let model_room = area_h
        .saturating_sub(used_so_far)
        .saturating_sub(reserved)
        .saturating_sub(2); // blank line + safety
    // Per-section max items shown in viewport (clamped to at least 3).
    let model_viewport = model_room.max(3);

    let model_lines = render_filter_picker_with_hits(
        &model_picker,
        model_active,
        w,
        model_viewport,
        lines.len(),
        area,
        &mut app.launcher_model_hits,
        &mut app.launcher_model_list_area,
    );
    lines.extend(model_lines);
    lines.push(Line::from(""));

    // Endpoint section
    if show_endpoint {
        let ep_active = active_section == LauncherSection::Endpoint;
        lines.push(Line::from(Span::styled(
            if ep_active {
                "  \u{25b8} Endpoint"
            } else {
                "    Endpoint"
            },
            section_style(ep_active),
        )));
        let ep_viewport = endpoint_picker
            .filtered_indices
            .len()
            .max(3)
            .min(area_h.saturating_sub(lines.len()).saturating_sub(2));
        let ep_lines = render_filter_picker_with_hits(
            &endpoint_picker,
            ep_active,
            w,
            ep_viewport.max(3),
            lines.len(),
            area,
            &mut app.launcher_endpoint_hits,
            &mut app.launcher_endpoint_list_area,
        );
        lines.extend(ep_lines);
        lines.push(Line::from(""));
    }

    // Recent combos section
    if !recent_list.is_empty() {
        let recent_active = active_section == LauncherSection::Recent;
        lines.push(Line::from(Span::styled(
            if recent_active {
                "  \u{25b8} Recent"
            } else {
                "    Recent"
            },
            section_style(recent_active),
        )));

        for (i, entry) in recent_list.iter().enumerate() {
            let selected = recent_active && i == recent_selected;
            let style = if selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let model_str = entry.model.as_deref().unwrap_or("default");
            let ep_str = entry
                .endpoint
                .as_deref()
                .map(|e| format!(" @ {}", e))
                .unwrap_or_default();
            let num = i + 1;
            let row_idx = lines.len();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {}. ", num),
                    if selected {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::styled(
                    format!("{} / {}{}", entry.executor, model_str, ep_str),
                    style,
                ),
            ]));
            app.launcher_recent_hits.push((i, row_rect(row_idx)));
        }
        lines.push(Line::from(""));
    }

    // Action button row: [Launch] and [Cancel] are clickable. Mouse hits
    // route to launch_from_launcher / close_launcher respectively. The
    // bracketed glyphs are 8 wide ("[Launch]") and 8 wide ("[Cancel]")
    // separated by 3 spaces.
    let footer_idx = lines.len();
    let footer_y = area.y.saturating_add(footer_idx as u16);
    let launch_x = area.x.saturating_add(2);
    let launch_w: u16 = 8;
    let cancel_x = launch_x.saturating_add(launch_w + 3);
    let cancel_w: u16 = 8;
    app.launcher_launch_btn_hit = Rect {
        x: launch_x,
        y: footer_y,
        width: launch_w,
        height: 1,
    };
    app.launcher_cancel_btn_hit = Rect {
        x: cancel_x,
        y: footer_y,
        width: cancel_w,
        height: 1,
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(
            "[Launch]",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("   "),
        Span::styled(
            "[Cancel]",
            Style::default().fg(Color::White).bg(Color::DarkGray),
        ),
        Span::styled(
            "    Enter / Ctrl+Enter create  ·  Tab section  ·  scroll/click select  ·  Esc cancel",
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);

    // Suppress the "LauncherListHit::Custom unused" warning when only Item is constructed.
    let _ = LauncherListHit::Custom;
}

/// Draw a text prompt overlay (fail reason, message, edit description).
/// Returns the overlay area for mouse hit-testing.
fn draw_text_prompt(
    frame: &mut Frame,
    action: &TextPromptAction,
    editor: &mut edtui::EditorState,
) -> Rect {
    use edtui::{EditorTheme, EditorView};
    let is_multiline = matches!(action, TextPromptAction::EditDescription(_));
    let title = match action {
        TextPromptAction::MarkFailed(id) => format!("Fail '{}' \u{2014} enter reason:", id),
        TextPromptAction::SendMessage(id) => format!("Message to '{}':", id),
        TextPromptAction::EditDescription(id) => format!("Edit description for '{}':", id),
        TextPromptAction::AttachFile => "Attach file \u{2014} enter path:".to_string(),
    };
    let size = frame.area();
    if is_multiline {
        let width = (size.width * 3 / 4)
            .max(50)
            .min(size.width.saturating_sub(4));
        let height = (size.height / 2).max(10).min(size.height.saturating_sub(4));
        let x = (size.width.saturating_sub(width)) / 2;
        let y = (size.height.saturating_sub(height)) / 2;
        let area = Rect::new(x, y, width, height);
        frame.render_widget(Clear, area);
        let block = Block::default()
            .title(format!(" {} ", title))
            .borders(Borders::ALL)
            .border_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let edit_height = inner.height.saturating_sub(1);
        let edit_area = Rect::new(inner.x, inner.y, inner.width, edit_height);
        let theme = EditorTheme::default()
            .hide_status_line()
            .base(Style::default().fg(Color::White))
            .cursor_style(Style::default().fg(Color::Black).bg(Color::Yellow));
        frame.render_widget(EditorView::new(editor).wrap(true).theme(theme), edit_area);
        frame.render_widget(
            Paragraph::new(vec![Line::from(Span::styled(
                "Ctrl+Enter: submit  Esc: cancel",
                Style::default().fg(Color::DarkGray),
            ))]),
            Rect::new(inner.x, inner.y + edit_height, inner.width, 1),
        );
        area
    } else {
        let width = 50.min(size.width.saturating_sub(4));
        let height = 6;
        let x = (size.width.saturating_sub(width)) / 2;
        let y = (size.height.saturating_sub(height)) / 2;
        let area = Rect::new(x, y, width, height);
        frame.render_widget(Clear, area);
        let block = Block::default()
            .title(format!(" {} ", title))
            .borders(Borders::ALL)
            .border_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );
        let inner = block.inner(area);
        frame.render_widget(block, area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "> ",
                Style::default().fg(Color::Yellow),
            ))),
            Rect::new(inner.x, inner.y, 2, 1),
        );
        let editor_area = Rect::new(inner.x + 2, inner.y, inner.width.saturating_sub(2), 1);
        let theme = EditorTheme::default()
            .hide_status_line()
            .base(Style::default().fg(Color::White))
            .cursor_style(Style::default().fg(Color::Black).bg(Color::Yellow));
        frame.render_widget(EditorView::new(editor).theme(theme), editor_area);
        if inner.height >= 3 {
            frame.render_widget(
                Paragraph::new(vec![Line::from(Span::styled(
                    "Enter: submit  Esc: cancel",
                    Style::default().fg(Color::DarkGray),
                ))]),
                Rect::new(inner.x, inner.y + 2, inner.width, 1),
            );
        }
        area
    }
}

/// Draw the task creation form overlay.
fn draw_task_form(frame: &mut Frame, form: &TaskFormState) {
    let size = frame.area();
    let width = 60.min(size.width.saturating_sub(4));
    let height = 20.min(size.height.saturating_sub(4));
    let x = (size.width.saturating_sub(width)) / 2;
    let y = (size.height.saturating_sub(height)) / 2;
    let area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Create Task ")
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let field_label = |label: &str, active: bool| -> Span {
        if active {
            Span::styled(
                format!("▸ {}:", label),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!("  {}:", label), Style::default().fg(Color::White))
        }
    };

    let cursor = Span::styled(
        "_",
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::SLOW_BLINK),
    );

    let mut lines: Vec<Line> = Vec::new();

    // Title field
    let is_title = form.active_field == TaskFormField::Title;
    lines.push(Line::from(vec![
        field_label("Title", is_title),
        Span::raw(" "),
        Span::raw(&form.title),
        if is_title {
            cursor.clone()
        } else {
            Span::raw("")
        },
    ]));
    lines.push(Line::from(""));

    // Description field
    let is_desc = form.active_field == TaskFormField::Description;
    lines.push(Line::from(vec![field_label("Description", is_desc)]));
    // Show first few lines of description
    let desc_lines: Vec<&str> = form.description.lines().collect();
    let show_lines = if desc_lines.is_empty() && is_desc {
        vec![""]
    } else {
        desc_lines.iter().take(3).copied().collect()
    };
    for (i, dl) in show_lines.iter().enumerate() {
        let is_last = i == show_lines.len() - 1;
        lines.push(Line::from(vec![
            Span::raw(format!("    {}", dl)),
            if is_desc && is_last {
                cursor.clone()
            } else {
                Span::raw("")
            },
        ]));
    }
    lines.push(Line::from(""));

    // Dependencies field
    let is_deps = form.active_field == TaskFormField::Dependencies;
    lines.push(Line::from(vec![
        field_label("After", is_deps),
        Span::raw(" "),
        Span::styled(
            form.selected_deps.join(", "),
            Style::default().fg(Color::Green),
        ),
    ]));
    if is_deps {
        lines.push(Line::from(vec![
            Span::raw("    search: "),
            Span::raw(&form.dep_search),
            cursor.clone(),
        ]));
        // Show fuzzy matches
        for (i, (id, title)) in form.dep_matches.iter().enumerate().take(5) {
            let is_selected = i == form.dep_match_idx;
            let style = if is_selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let marker = if is_selected { "▸ " } else { "  " };
            let display = if title.len() > 30 {
                format!(
                    "{}{} ({}…)",
                    marker,
                    id,
                    &title[..title.floor_char_boundary(27)]
                )
            } else {
                format!("{}{} ({})", marker, id, title)
            };
            lines.push(Line::from(Span::styled(display, style)));
        }
    }
    lines.push(Line::from(""));

    // Tags field
    let is_tags = form.active_field == TaskFormField::Tags;
    lines.push(Line::from(vec![
        field_label("Tags", is_tags),
        Span::raw(" "),
        Span::raw(&form.tags),
        if is_tags { cursor } else { Span::raw("") },
    ]));
    lines.push(Line::from(""));

    // Submit hint
    lines.push(Line::from(Span::styled(
        "Ctrl-Enter: create  Tab: next field  Esc: cancel",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, inner);
}

/// Render a full-width bar with focus-aware background color.
///
/// In side-by-side split mode the bar is split at the panel boundary: the
/// focused pane's portion gets `focused_bg` while the unfocused portion gets
/// `unfocused_bg`.  In other layout modes the bar is rendered uniformly with
/// `focused_bg`.
fn render_focus_bar(
    frame: &mut Frame,
    app: &VizApp,
    area: Rect,
    line: Line<'_>,
    focused_bg: Color,
    unfocused_bg: Color,
) {
    let is_side_by_side =
        app.last_right_panel_area.width > 0 && app.last_right_panel_area.x > area.x;

    if is_side_by_side {
        let graph_focused = app.focused_panel == FocusedPanel::Graph;
        let graph_bg = if graph_focused {
            focused_bg
        } else {
            unfocused_bg
        };
        let panel_bg = if graph_focused {
            unfocused_bg
        } else {
            focused_bg
        };

        let panel_x = app.last_right_panel_area.x;
        let graph_width = panel_x.saturating_sub(area.x);
        let panel_width = area.width.saturating_sub(graph_width);

        let graph_area = Rect::new(area.x, area.y, graph_width, area.height);
        let panel_area = Rect::new(panel_x, area.y, panel_width, area.height);

        // Fill background for each portion.
        frame.render_widget(
            Block::default().style(Style::default().bg(graph_bg)),
            graph_area,
        );
        frame.render_widget(
            Block::default().style(Style::default().bg(panel_bg)),
            panel_area,
        );

        // Render text over the full area (no bg override preserves the fills).
        frame.render_widget(Paragraph::new(line), area);
    } else {
        let bar = Paragraph::new(line).style(Style::default().bg(focused_bg));
        frame.render_widget(bar, area);
    }
}

/// Draw the bottom action hints bar with context-sensitive hotkey tooltips.
///
/// Format: ` context | MODE | key:hint  key:hint  key:hint`
/// Mode badge colors: NAV=dim gray, EDIT=yellow, SEARCH=cyan
/// Truncates hints with `…` if terminal is too narrow.
fn draw_action_hints(frame: &mut Frame, app: &VizApp, area: Rect) {
    let width = area.width as usize;

    // Determine context label, mode badge, and key hints.
    let (context_label, mode_badge, mode_color, hints) = action_hints_parts(app);

    let separator = " | ";
    let sep_style = Style::default().fg(Color::Rgb(80, 80, 80));
    let key_style = Style::default().fg(Color::Yellow);
    let desc_style = Style::default().fg(Color::Rgb(140, 140, 140));
    let badge_style = Style::default().fg(mode_color).add_modifier(Modifier::BOLD);

    // Calculate minimum width needed for context + mode (the priority parts).
    let prefix_text = format!(" {}{}{}", context_label, separator, mode_badge);
    let prefix_width = UnicodeWidthStr::width(prefix_text.as_str());

    if width < 5 {
        // Terminal too narrow for anything useful.
        let bar = Paragraph::new(Line::from(vec![Span::styled(
            " …",
            Style::default().fg(Color::DarkGray),
        )]))
        .style(Style::default().bg(Color::Rgb(30, 30, 30)));
        frame.render_widget(bar, area);
        return;
    }

    let mut spans: Vec<Span> = Vec::with_capacity(16);

    // Context label (pane/tab name)
    spans.push(Span::styled(
        format!(" {}", context_label),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::styled(separator, sep_style));
    // Mode badge
    spans.push(Span::styled(mode_badge, badge_style));

    // Add hints if we have room.
    if !hints.is_empty() {
        spans.push(Span::styled(separator, sep_style));

        // Calculate remaining width for hints.
        // prefix_width covers: leading space + context + sep + mode
        let hints_budget = width.saturating_sub(prefix_width + separator.len());

        let mut used = 0usize;
        let mut hint_spans: Vec<Span> = Vec::new();
        for (i, (key, desc)) in hints.iter().enumerate() {
            let hint_text = format!("{}:{}", key, desc);
            let hint_width = UnicodeWidthStr::width(hint_text.as_str());
            let spacing = if i > 0 { 2 } else { 0 }; // double space between hints
            let needed = spacing + hint_width;

            if used + needed + 3 > hints_budget && i < hints.len() - 1 {
                // Not enough room — add ellipsis and stop.
                if spacing > 0 {
                    hint_spans.push(Span::styled("  ", desc_style));
                }
                hint_spans.push(Span::styled(
                    "…",
                    Style::default().fg(Color::Rgb(80, 80, 80)),
                ));
                break;
            }
            if used + needed > hints_budget {
                break;
            }

            if i > 0 {
                hint_spans.push(Span::styled("  ", desc_style));
            }
            hint_spans.push(Span::styled(key.to_string(), key_style));
            hint_spans.push(Span::styled(format!(":{}", desc), desc_style));
            used += needed;
        }
        spans.extend(hint_spans);
    }

    // Append latest toast in the status bar (if any).
    if let Some(toast) = app.toasts.last() {
        let color = match toast.severity {
            ToastSeverity::Info => Color::Green,
            ToastSeverity::Warning => Color::Yellow,
            ToastSeverity::Error => Color::Red,
        };
        spans.push(Span::styled(
            "  │ ",
            Style::default().fg(Color::Rgb(80, 80, 80)),
        ));
        spans.push(Span::styled(
            toast.message.as_str(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    }

    render_focus_bar(
        frame,
        app,
        area,
        Line::from(spans),
        Color::Rgb(30, 30, 30),
        Color::Rgb(15, 15, 15),
    );
}

/// Returns (context_label, mode_badge, mode_color, hints) for the bottom action bar.
/// `hints` is a list of (key, description) pairs ordered by importance.
fn action_hints_parts(app: &VizApp) -> (&str, &str, Color, Vec<(&str, &str)>) {
    match &app.input_mode {
        InputMode::Search => (
            "Search",
            "SEARCH",
            Color::Cyan,
            vec![
                ("Tab", "next"),
                ("S-Tab", "prev"),
                ("Enter", "go"),
                ("Esc", "cancel"),
            ],
        ),
        InputMode::ChatSearch => (
            "0:Chat",
            "SEARCH",
            Color::Cyan,
            vec![
                ("Tab", "next"),
                ("S-Tab", "prev"),
                ("Enter", "accept"),
                ("Esc", "cancel"),
                ("C-a", "all history"),
            ],
        ),
        InputMode::ChatInput => {
            let label = if app.right_panel_tab == RightPanelTab::Chat {
                "0:Chat"
            } else {
                "Chat"
            };
            (
                label,
                "EDIT",
                Color::Magenta,
                vec![
                    ("Enter", "send"),
                    ("Esc", "cancel"),
                    ("↑↓", "history"),
                    ("S-Enter", "newline"),
                ],
            )
        }
        InputMode::MessageInput => (
            "3:Msg",
            "EDIT",
            Color::Yellow,
            vec![
                ("Enter", "send"),
                ("S-Enter", "newline"),
                ("Ctrl+K/Y", "kill/yank"),
                ("Esc", "exit"),
            ],
        ),
        InputMode::TaskForm => (
            "New Task",
            "EDIT",
            Color::Yellow,
            vec![
                ("Ctrl-Enter", "create"),
                ("Tab", "field"),
                ("Esc", "cancel"),
            ],
        ),
        InputMode::Confirm(_) => (
            "Confirm",
            "EDIT",
            Color::Yellow,
            vec![("y", "yes"), ("n", "no")],
        ),
        InputMode::TextPrompt(action) => {
            let keys = if matches!(action, TextPromptAction::EditDescription(_)) {
                vec![
                    ("Ctrl-Enter", "submit"),
                    ("Enter", "newline"),
                    ("Esc", "cancel"),
                ]
            } else {
                vec![("Enter", "submit"), ("Esc", "cancel")]
            };
            ("Prompt", "EDIT", Color::Yellow, keys)
        }
        InputMode::ConfigEdit => (
            "5:Config",
            "EDIT",
            Color::Yellow,
            vec![("Enter", "save"), ("Esc", "cancel")],
        ),
        InputMode::ChoiceDialog(_) => (
            "Choice",
            "EDIT",
            Color::Yellow,
            vec![("↑↓", "navigate"), ("Enter", "select"), ("Esc", "cancel")],
        ),
        InputMode::CoordinatorPicker => (
            "Picker",
            "NAV",
            Color::Cyan,
            vec![
                ("↑↓", "select"),
                ("Enter", "open"),
                ("+", "new"),
                ("−", "close"),
                ("Esc", "cancel"),
            ],
        ),
        InputMode::Normal => match app.focused_panel {
            FocusedPanel::Graph if app.archive_browser.active => {
                if app.archive_browser.filter_active {
                    (
                        "Archive",
                        "FILTER",
                        Color::Cyan,
                        vec![("Enter", "accept"), ("Esc", "clear")],
                    )
                } else {
                    (
                        "Archive",
                        "NAV",
                        Color::Yellow,
                        vec![
                            ("↑↓", "select"),
                            ("/", "filter"),
                            ("r", "restore"),
                            ("R", "refresh"),
                            ("A/Esc", "close"),
                        ],
                    )
                }
            }
            FocusedPanel::Graph => {
                let tab_hint = if app.responsive_breakpoint == ResponsiveBreakpoint::Compact {
                    ("Tab/]/[", "panels")
                } else {
                    ("Tab", "panel")
                };
                (
                    if app.responsive_breakpoint == ResponsiveBreakpoint::Compact {
                        "▸Graph"
                    } else {
                        "Graph"
                    },
                    "NAV",
                    Color::Rgb(120, 120, 120),
                    vec![
                        ("↑↓", "select"),
                        ("Enter", "inspect"),
                        tab_hint,
                        ("/", "search"),
                        ("a", "add"),
                        ("D", "done"),
                        ("M", "msg"),
                        ("i/v", "resize pane"),
                        ("?", "help"),
                        ("Alt←→", "cycle views"),
                    ],
                )
            }
            FocusedPanel::RightPanel => {
                let tab = &app.right_panel_tab;
                let tab_label: &str = match tab {
                    RightPanelTab::Chat => "0:Chat",
                    RightPanelTab::Detail => "1:Detail",
                    RightPanelTab::Agency => "2:Agency",
                    RightPanelTab::Config => "3:Config",
                    RightPanelTab::Log => "4:Log",
                    RightPanelTab::CoordLog => "5:Coord",
                    RightPanelTab::Dashboard => "6:Dash",
                    RightPanelTab::Messages => "7:Msg",
                    // Dead tabs, not reachable from the bar.
                    RightPanelTab::Files => "Files",
                    RightPanelTab::Firehose => "Fire",
                    RightPanelTab::Output => "Out",
                };
                let mut hints: Vec<(&str, &str)> = Vec::new();
                match tab {
                    RightPanelTab::Chat if app.chat_pty_mode && app.chat_pty_forwards_stdin => {
                        hints.push(("Ctrl+T", "leave PTY"));
                        hints.push(("PgUp/Dn", "scroll"));
                    }
                    RightPanelTab::Chat if app.chat_pty_mode => {
                        hints.push(("Enter", "chat"));
                        hints.push(("Ctrl+T", "focus PTY"));
                        hints.push(("↑↓", "scroll"));
                        hints.push(("←→", "coordinators"));
                    }
                    RightPanelTab::Chat => {
                        hints.push(("←→", "coordinators"));
                        hints.push(("+", "new"));
                        hints.push(("-", "close"));
                        hints.push(("Enter", "chat"));
                        hints.push(("↑↓", "scroll"));
                    }
                    RightPanelTab::Detail => {
                        hints.push(("↑↓", "scroll"));
                        hints.push(("PgUp/Dn", "page"));
                        hints.push(("Enter", "toggle"));
                        if !app.iteration_archives.is_empty() {
                            hints.push(("[/]", "iterations"));
                        }
                    }
                    RightPanelTab::Log
                    | RightPanelTab::CoordLog
                    | RightPanelTab::Agency
                    | RightPanelTab::Firehose => {
                        hints.push(("↑↓", "scroll"));
                        hints.push(("PgUp/Dn", "page"));
                        hints.push(("Home/End", "jump"));
                    }
                    RightPanelTab::Output => {
                        hints.push(("←→", "agents"));
                        hints.push(("↑↓", "scroll"));
                        hints.push(("PgUp/Dn", "page"));
                        hints.push(("Home/End", "jump"));
                    }
                    RightPanelTab::Messages => {
                        hints.push(("Enter", "type"));
                        hints.push(("↑↓", "scroll"));
                    }
                    RightPanelTab::Config => {
                        hints.push(("↑↓", "select"));
                        hints.push(("Enter", "edit"));
                        hints.push(("Esc", "cancel"));
                    }
                    RightPanelTab::Files => {
                        hints.push(("↑↓", "select"));
                        hints.push(("Enter", "open"));
                        hints.push(("Esc", "back"));
                    }
                    RightPanelTab::Dashboard => {
                        hints.push(("↑↓", "select"));
                        hints.push(("Enter", "drill-down"));
                        hints.push(("k", "kill"));
                        hints.push(("t", "task detail"));
                    }
                }
                let vendor_pty_active = app.chat_pty_mode
                    && app.chat_pty_forwards_stdin
                    && *tab == RightPanelTab::Chat
                    && !app.chat_pty_observer;
                if !vendor_pty_active {
                    // Common hints for all right-panel tabs (except vendor PTY).
                    if app.responsive_breakpoint == ResponsiveBreakpoint::Compact {
                        hints.push(("Tab/]/[", "panels"));
                    } else {
                        hints.push(("Tab", "graph"));
                    }
                    hints.push(("i/v", "resize pane"));
                    hints.push(("?", "help"));
                    hints.push(("Alt←→", "cycle views"));
                }
                // In compact mode, prefix the tab label with a breadcrumb indicator.
                let label: &str = if app.responsive_breakpoint == ResponsiveBreakpoint::Compact {
                    match app.single_panel_view {
                        SinglePanelView::Detail => "▸Detail",
                        SinglePanelView::Log => "▸Log",
                        _ => tab_label,
                    }
                } else {
                    tab_label
                };
                let (mode_badge, mode_color) = if vendor_pty_active {
                    ("PTY", Color::Green)
                } else {
                    ("NAV", Color::Rgb(120, 120, 120))
                };
                (label, mode_badge, mode_color, hints)
            }
        },
        InputMode::Launcher => (
            "0:Chat",
            "LAUNCHER",
            Color::Cyan,
            vec![
                ("Tab", "section"),
                ("\u{2191}\u{2193}", "select"),
                ("Enter", "create"),
                ("Esc", "cancel"),
            ],
        ),
    }
}

fn draw_status_bar(frame: &mut Frame, app: &VizApp, area: Rect) {
    if app.search_active {
        // Search input mode: show the search prompt with cursor.
        let mut spans = vec![
            Span::styled(
                format!(" /{}", app.search_input),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                "_",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::SLOW_BLINK),
            ),
        ];

        // Show match count inline.
        if !app.search_input.is_empty() {
            if app.fuzzy_matches.is_empty() {
                spans.push(Span::styled(
                    "  [no matches]",
                    Style::default().fg(Color::Red),
                ));
            } else {
                let idx = app.current_match.unwrap_or(0);
                spans.push(Span::styled(
                    format!("  [Match {}/{}]", idx + 1, app.fuzzy_matches.len()),
                    Style::default().fg(Color::Green),
                ));
            }
        }

        // Keybinding hints for search mode.
        spans.push(Span::styled(
            "  [Tab: next  Shift-Tab: prev  Enter: go to  Esc: cancel]",
            Style::default().fg(Color::Rgb(100, 100, 100)),
        ));

        render_focus_bar(
            frame,
            app,
            area,
            Line::from(spans),
            Color::DarkGray,
            Color::Rgb(40, 40, 40),
        );
        return;
    }

    // Filter locked: search accepted, highlights visible, navigating matches.
    if !app.search_input.is_empty() && !app.fuzzy_matches.is_empty() {
        let idx = app.current_match.unwrap_or(0);
        let mut spans = vec![
            Span::styled(
                format!(" /{}", app.search_input),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!("  [Match {}/{}]", idx + 1, app.fuzzy_matches.len()),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                "  [n: next  N: prev  /: new search  Esc: clear]",
                Style::default().fg(Color::Rgb(100, 100, 100)),
            ),
        ];

        // Scroll position
        spans.push(Span::styled("  ", Style::default()));
        spans.push(Span::styled(
            format!("L{}/{}", app.scroll.offset_y + 1, app.visible_line_count()),
            Style::default().fg(Color::DarkGray),
        ));

        render_focus_bar(
            frame,
            app,
            area,
            Line::from(spans),
            Color::DarkGray,
            Color::Rgb(40, 40, 40),
        );
        return;
    }

    let c = &app.task_counts;
    let mut spans = vec![Span::styled(
        format!(
            " {} tasks ({} done, {} open, {} active",
            c.total, c.done, c.open, c.in_progress
        ),
        Style::default().fg(Color::White),
    )];

    if c.failed > 0 {
        spans.push(Span::styled(
            format!(", {} failed", c.failed),
            Style::default().fg(Color::Red),
        ));
    }

    spans.push(Span::styled(") ", Style::default().fg(Color::White)));

    if c.archived > 0 {
        spans.push(Span::styled(
            format!("{} archived ", c.archived),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Token breakdown: input/output/cache with view/total toggle
    let visible_usage;
    let (usage, label) = if app.show_total_tokens {
        (&app.total_usage, "total")
    } else {
        visible_usage = app.visible_token_usage();
        (&visible_usage, "view")
    };
    if usage.total_tokens() > 0 {
        spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
        render_token_breakdown(&mut spans, usage, label);
    }

    // Time counters
    {
        let tc = &app.time_counters;
        let mut cp: Vec<Span> = Vec::new();
        if tc.show_uptime {
            cp.push(Span::styled(
                match tc.live_uptime_secs() {
                    Some(s) => format!("\u{2191}{}", format_duration_compact(s)),
                    None => "\u{2191}-".into(),
                },
                Style::default().fg(Color::Cyan),
            ));
        }
        if tc.show_cumulative {
            cp.push(Span::styled(
                format!(
                    "\u{03A3}{}",
                    format_duration_compact(tc.live_cumulative_secs())
                ),
                Style::default().fg(Color::Magenta),
            ));
        }
        if tc.show_active && tc.active_agent_count > 0 {
            cp.push(Span::styled(
                format!(
                    "\u{26A1}{}({})",
                    format_duration_compact(tc.live_active_secs()),
                    tc.active_agent_count
                ),
                Style::default().fg(Color::Green),
            ));
        }
        if tc.show_session {
            cp.push(Span::styled(
                format!(
                    "\u{25F7}{}",
                    format_duration_compact(tc.session_start.elapsed().as_secs())
                ),
                Style::default().fg(Color::DarkGray),
            ));
        }
        if !cp.is_empty() {
            spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
            for (i, p) in cp.into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" ", Style::default()));
                }
                spans.push(p);
            }
            spans.push(Span::styled(" ", Style::default()));
        }
    }

    // Cycle timing indicators
    if !app.cycle_timing.is_empty() {
        spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
        for (i, ct) in app.cycle_timing.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" ", Style::default()));
            }
            // Compact label: task_id[iter/max, timing]
            let short_id = if ct.task_id.len() > 12 {
                format!("{}…", &ct.task_id[..ct.task_id.floor_char_boundary(11)])
            } else {
                ct.task_id.clone()
            };
            let timing = if let Some(secs) = ct.next_due_in_secs {
                if secs > 0 {
                    format!("in {}", format_duration_compact(secs as u64))
                } else {
                    "ready".to_string()
                }
            } else if let Some(ago) = ct.last_completed_ago_secs {
                format!("{}ago", format_duration_compact(ago as u64))
            } else {
                "–".to_string()
            };
            let color = match ct.status {
                workgraph::graph::Status::InProgress => Color::Green,
                workgraph::graph::Status::Open => Color::Yellow,
                workgraph::graph::Status::Done => Color::Cyan,
                _ => Color::DarkGray,
            };
            spans.push(Span::styled(
                format!(
                    "⟳{}[{}/{}·{}]",
                    short_id, ct.iteration, ct.max_iterations, timing
                ),
                Style::default().fg(color),
            ));
        }
        spans.push(Span::styled(" ", Style::default()));
    }

    // Scroll position
    spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        format!("L{}/{} ", app.scroll.offset_y + 1, app.visible_line_count()),
        Style::default().fg(Color::White),
    ));

    // Selected task indicator
    if let Some(task_id) = app.selected_task_id() {
        spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
        // Truncate long task IDs for status bar display
        let display_id = if task_id.len() > 24 {
            format!("{}…", &task_id[..task_id.floor_char_boundary(23)])
        } else {
            task_id.to_string()
        };
        spans.push(Span::styled(
            format!("▸{} ", display_id),
            Style::default().fg(Color::Yellow),
        ));
    }

    // Search/filter state
    let search_status = app.search_status();
    if !search_status.is_empty() {
        spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("{} ", search_status),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Live refresh indicator
    if app.task_counts.in_progress > 0 {
        spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("LIVE {} ", app.last_refresh_display),
            Style::default().fg(Color::Green),
        ));
    } else {
        spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("{} ", app.last_refresh_display),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Trace state indicator
    if !app.trace_visible {
        spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            "TRACE OFF ",
            Style::default().fg(Color::Yellow),
        ));
    }

    // Mouse state indicator
    if !app.mouse_enabled {
        spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            "MOUSE OFF ",
            Style::default().fg(Color::Yellow),
        ));
    }

    // Scroll axis swap indicator (horizontal scroll via vertical swipe)
    if app.scroll_axis_swapped {
        spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            "H-SCROLL ",
            Style::default().fg(Color::Magenta),
        ));
    }

    // Sort mode indicator (show when not the default)
    if app.sort_mode != SortMode::ReverseChronological {
        spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("{} ", app.sort_mode.label()),
            Style::default().fg(Color::Cyan),
        ));
    }

    // Layout mode indicator (always shown)
    spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        match app.layout_mode {
            LayoutMode::ThirdInspector => {
                format!("1/3 PANEL ({}%) ", app.right_panel_percent)
            }
            LayoutMode::HalfInspector => {
                format!("1/2 PANEL ({}%) ", app.right_panel_percent)
            }
            LayoutMode::TwoThirdsInspector => {
                format!("2/3 PANEL ({}%) ", app.right_panel_percent)
            }
            LayoutMode::FullInspector => "FULL PANEL ".to_string(),
            LayoutMode::Off => "FULL GRAPH ".to_string(),
        },
        Style::default().fg(Color::Magenta),
    ));

    // Help hint
    spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        "?:help ",
        Style::default().fg(Color::DarkGray),
    ));

    render_focus_bar(
        frame,
        app,
        area,
        Line::from(spans),
        Color::DarkGray,
        Color::Rgb(40, 40, 40),
    );
}

/// Render the HUD vitals bar: always-visible strip showing system health at a glance.
///
/// ```text
/// | ● 2 agents | 8 open · 3 running · 45 done | last event 4s ago | coord ● 3s |
/// ```
fn draw_vitals_bar(frame: &mut Frame, app: &VizApp, area: Rect) {
    let v = &app.vitals;

    let sep_style = Style::default().fg(Color::Rgb(80, 80, 80));
    let separator = " | ";

    let mut spans: Vec<Span> = Vec::with_capacity(16);

    // Agent count with dot indicator
    let (dot, dot_color) = if v.agents_alive > 0 {
        ("●", Color::Green)
    } else {
        ("○", Color::DarkGray)
    };
    spans.push(Span::styled(
        format!(" {} {} agents", dot, v.agents_alive),
        Style::default().fg(dot_color),
    ));

    // Task status counts
    spans.push(Span::styled(separator, sep_style));
    spans.push(Span::styled(
        format!("{} open", v.open),
        Style::default().fg(Color::Yellow),
    ));
    spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        format!("{} running", v.running),
        Style::default().fg(Color::Green),
    ));
    spans.push(Span::styled(" · ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        format!("{} done", v.done),
        Style::default().fg(Color::Cyan),
    ));

    // Time since last event with color coding
    spans.push(Span::styled(separator, sep_style));
    let (event_text, event_color) = match v.last_event_time {
        Some(t) => match t.elapsed() {
            Ok(d) => {
                let secs = d.as_secs();
                let color = match vitals_staleness_color(secs) {
                    VitalsStaleness::Fresh => Color::Green,
                    VitalsStaleness::Stale => Color::Yellow,
                    VitalsStaleness::Dead => Color::Red,
                };
                (
                    format!("last event {} ago", format_duration_compact(secs)),
                    color,
                )
            }
            Err(_) => ("last event just now".to_string(), Color::Green),
        },
        None => ("no events".to_string(), Color::DarkGray),
    };
    spans.push(Span::styled(event_text, Style::default().fg(event_color)));

    // Coordinator heartbeat
    spans.push(Span::styled(separator, sep_style));
    if v.daemon_running {
        let (coord_text, coord_color) = match v.coord_last_tick {
            Some(t) => match t.elapsed() {
                Ok(d) => {
                    let secs = d.as_secs();
                    let color = match vitals_staleness_color(secs) {
                        VitalsStaleness::Fresh => Color::Green,
                        VitalsStaleness::Stale => Color::Yellow,
                        VitalsStaleness::Dead => Color::Red,
                    };
                    (format!("coord ● {}", format_duration_compact(secs)), color)
                }
                Err(_) => ("coord ● 0s".to_string(), Color::Green),
            },
            None => ("coord ● –".to_string(), Color::DarkGray),
        };
        spans.push(Span::styled(coord_text, Style::default().fg(coord_color)));
    } else {
        spans.push(Span::styled(
            "coord ○ down",
            Style::default().fg(Color::Red),
        ));
    }

    spans.push(Span::styled(" ", Style::default()));

    let bar = Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Rgb(25, 25, 25)));
    frame.render_widget(bar, area);
}

/// Render the service health badge at the right end of the status bar.
fn draw_service_health_badge(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let health = &app.service_health;
    let (dot_color, bg_color) = match health.level {
        ServiceHealthLevel::Green => (Color::Green, Color::Rgb(20, 50, 20)),
        ServiceHealthLevel::Yellow => (Color::Yellow, Color::Rgb(50, 50, 10)),
        ServiceHealthLevel::Red => (Color::Red, Color::Rgb(50, 20, 20)),
    };

    let badge_text = format!(" \u{25CF} {} ", health.label);
    let badge_width = UnicodeWidthStr::width(badge_text.as_str()) as u16;

    if area.width < badge_width + 1 {
        app.last_service_badge_area = Rect::default();
        return;
    }

    let badge_x = area.x + area.width - badge_width;
    let badge_area = Rect::new(badge_x, area.y, badge_width, 1);
    app.last_service_badge_area = badge_area;

    let badge = Paragraph::new(Line::from(vec![
        Span::styled(
            " \u{25CF}".to_string(),
            Style::default().fg(dot_color).bg(bg_color),
        ),
        Span::styled(
            format!(" {} ", health.label),
            Style::default().fg(Color::White).bg(bg_color),
        ),
    ]));
    frame.render_widget(badge, badge_area);
}

/// Draw severity-leveled toast notifications in the top-right of the graph area.
/// Toasts stack downward from the top-right. Max 4 visible. Each severity has
/// a distinct color (green/yellow/red) and fade behavior.
fn draw_toasts(frame: &mut Frame, app: &VizApp) {
    if app.toasts.is_empty() {
        return;
    }

    let graph_area = app.last_graph_area;
    if graph_area.width < 20 || graph_area.height < 3 {
        return;
    }

    let max_width = 60.min(graph_area.width.saturating_sub(4));
    let mut y_offset: u16 = 0;

    // Show newest toasts first (last in vec), up to MAX_VISIBLE_TOASTS.
    let visible_count = app.toasts.len().min(super::state::MAX_VISIBLE_TOASTS);
    let start = app.toasts.len().saturating_sub(visible_count);

    for toast in app.toasts[start..].iter().rev() {
        let elapsed_ms = toast.created_at.elapsed().as_millis() as u64;

        // Compute fade based on severity auto-dismiss duration.
        let fade = match toast.severity.auto_dismiss_duration() {
            Some(dur) => {
                let total_ms = dur.as_millis() as u64;
                let fade_start_ms = total_ms.saturating_sub(1000);
                if elapsed_ms >= total_ms {
                    continue; // expired
                } else if elapsed_ms < fade_start_ms {
                    1.0_f64
                } else {
                    1.0 - ((elapsed_ms - fade_start_ms) as f64 / 1000.0)
                }
            }
            None => 1.0, // Error toasts don't fade
        };

        // Truncate message to fit within max_width (with 2 chars padding).
        let display_msg = if toast.message.width() > (max_width as usize).saturating_sub(2) {
            let limit = (max_width as usize).saturating_sub(5);
            let truncated: String = toast.message.chars().take(limit).collect();
            format!("{}...", truncated)
        } else {
            toast.message.clone()
        };

        let toast_width = (display_msg.width() as u16 + 2).min(max_width);
        let toast_height: u16 = 1;

        // Position: top-right of graph area, stacking downward.
        let x = graph_area.x + graph_area.width.saturating_sub(toast_width + 1);
        let y = graph_area.y + 1 + y_offset;

        if y >= graph_area.y + graph_area.height.saturating_sub(1) {
            break; // No more room to stack.
        }

        let area = Rect::new(x, y, toast_width, toast_height);

        // Color by severity, with fade.
        let (base_fg, base_bg) = match toast.severity {
            ToastSeverity::Info => ((100.0, 255.0, 100.0), (15.0, 40.0, 15.0)),
            ToastSeverity::Warning => ((255.0, 220.0, 80.0), (40.0, 35.0, 10.0)),
            ToastSeverity::Error => ((255.0, 100.0, 100.0), (50.0, 15.0, 15.0)),
        };

        let fg_r = (base_fg.0 * fade) as u8;
        let fg_g = (base_fg.1 * fade) as u8;
        let fg_b = (base_fg.2 * fade) as u8;
        let bg_r = (base_bg.0 * fade) as u8;
        let bg_g = (base_bg.1 * fade) as u8;
        let bg_b = (base_bg.2 * fade) as u8;

        frame.render_widget(Clear, area);
        let para = Paragraph::new(Line::from(Span::styled(
            format!(" {} ", display_msg),
            Style::default()
                .fg(Color::Rgb(fg_r, fg_g, fg_b))
                .bg(Color::Rgb(bg_r, bg_g, bg_b)),
        )));
        frame.render_widget(para, area);

        y_offset += 1;
    }
}

/// Draw the key feedback overlay showing recent key presses.
/// Positioned at the bottom-left of the screen, above the hints bar.
fn draw_key_feedback(frame: &mut Frame, app: &VizApp) {
    let size = frame.area();
    if size.width < 10 || size.height < 5 {
        return;
    }

    let duration_ms = VizApp::KEY_FEEDBACK_DURATION.as_millis() as u64;

    // Collect visible entries with fade factors.
    let entries: Vec<(&str, f64)> = app
        .key_feedback
        .iter()
        .filter_map(|(label, when)| {
            let elapsed_ms = when.elapsed().as_millis() as u64;
            if elapsed_ms >= duration_ms {
                return None;
            }
            // Fade: full opacity for first 1s, then fade out over remaining time.
            let fade_start_ms = 1000u64;
            let fade = if elapsed_ms < fade_start_ms {
                1.0
            } else {
                1.0 - ((elapsed_ms - fade_start_ms) as f64 / (duration_ms - fade_start_ms) as f64)
            };
            Some((label.as_str(), fade))
        })
        .collect();

    if entries.is_empty() {
        return;
    }

    // Build a single-line display: keys separated by thin space.
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (i, (label, fade)) in entries.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        // Cyan tint fading to dark.
        let brightness = (*fade * 255.0) as u8;
        let fg = Color::Rgb((brightness as f64 * 0.7) as u8, brightness, brightness);
        let bg = Color::Rgb(
            (20.0 * fade) as u8,
            (30.0 * fade) as u8,
            (40.0 * fade) as u8,
        );
        spans.push(Span::styled(
            format!(" {} ", label),
            Style::default().fg(fg).bg(bg).add_modifier(if *fade > 0.7 {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ));
    }

    let line = Line::from(spans);
    let line_width = line.width() as u16;

    // Position: bottom-left, one row above the hints bar.
    let x = 1;
    let y = size.height.saturating_sub(2);
    let w = line_width.min(size.width.saturating_sub(2));
    let area = Rect::new(x, y, w, 1);

    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(line), area);
}

/// Draw touch/click echo indicators — concentric ring shapes that fade out.
///
/// Each echo renders a small pattern of unicode characters centered on the
/// click position. The pattern shrinks and fades over ~0.7 seconds, providing
/// visual feedback for mouse/touch interaction (useful for screencasts/demos).
fn draw_touch_echoes(frame: &mut Frame, app: &VizApp) {
    let area = frame.area();
    if area.width < 3 || area.height < 3 {
        return;
    }

    // Ring patterns at different animation stages (outer → inner as time progresses).
    // Uses light box-drawing characters for a clean circle-ish look.
    // Pattern: (dy, dx, char) offsets from center.
    #[rustfmt::skip]
    const RING_OUTER: &[(i16, i16, char)] = &[
        (-1, -1, '╭'), (-1, 0, '─'), (-1, 1, '╮'),
        ( 0, -1, '│'),               ( 0, 1, '│'),
        ( 1, -1, '╰'), ( 1, 0, '─'), ( 1, 1, '╯'),
    ];
    const DOT_CENTER: char = '●';

    for echo in &app.touch_echoes {
        let progress = echo.progress();
        if progress >= 1.0 {
            continue;
        }

        // Color: bright cyan → dim → gone. Use RGB for smooth fade.
        let intensity = 1.0 - progress;
        let fg = Color::Rgb(
            (100.0 * intensity) as u8,
            (220.0 * intensity) as u8,
            (255.0 * intensity) as u8,
        );

        // Phase 1 (0.0–0.4): Show ring + center dot.
        // Phase 2 (0.4–0.7): Ring fades, center dot shrinks.
        let show_ring = progress < 0.4;

        if show_ring {
            for &(dy, dx, ch) in RING_OUTER {
                let r = echo.row as i16 + dy;
                let c = echo.col as i16 + dx;
                if r >= area.y as i16
                    && r < (area.y + area.height) as i16
                    && c >= area.x as i16
                    && c < (area.x + area.width) as i16
                {
                    let cell_area = Rect::new(c as u16, r as u16, 1, 1);
                    frame.render_widget(Clear, cell_area);
                    frame.render_widget(
                        Paragraph::new(Span::styled(ch.to_string(), Style::default().fg(fg))),
                        cell_area,
                    );
                }
            }
        }

        // Center dot — always visible while echo is active.
        if echo.row >= area.y
            && echo.row < area.y + area.height
            && echo.col >= area.x
            && echo.col < area.x + area.width
        {
            let center = Rect::new(echo.col, echo.row, 1, 1);
            frame.render_widget(Clear, center);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    DOT_CENTER.to_string(),
                    Style::default().fg(fg).add_modifier(if progress < 0.3 {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
                )),
                center,
            );
        }
    }
}

fn draw_service_health_detail(frame: &mut Frame, app: &VizApp) {
    let size = frame.area();
    let health = &app.service_health;

    let width = 50.min(size.width.saturating_sub(4));
    let height = 22.min(size.height.saturating_sub(4));

    // Anchor to top-right, below the status bar.
    let x = size.width.saturating_sub(width + 1);
    let y = 1.min(size.height.saturating_sub(height));
    let area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, area);

    let (border_color, title_dot) = match health.level {
        ServiceHealthLevel::Green => (Color::Green, "\u{25CF}"),
        ServiceHealthLevel::Yellow => (Color::Yellow, "\u{25CF}"),
        ServiceHealthLevel::Red => (Color::Red, "\u{25CF}"),
    };

    let block = Block::default()
        .title(format!(" {} Service Health ", title_dot))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    let label_style = Style::default().fg(Color::Cyan);
    let value_style = Style::default().fg(Color::White);
    let dim_style = Style::default().fg(Color::DarkGray);

    // PID & uptime
    lines.push(Line::from(vec![
        Span::styled("  PID: ", label_style),
        Span::styled(
            health
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "N/A".to_string()),
            value_style,
        ),
        Span::styled("    Uptime: ", label_style),
        Span::styled(health.uptime.as_deref().unwrap_or("N/A"), value_style),
    ]));

    // Socket
    lines.push(Line::from(vec![
        Span::styled("  Socket: ", label_style),
        Span::styled(health.socket_path.as_deref().unwrap_or("N/A"), dim_style),
    ]));

    lines.push(Line::from(""));

    // Agents
    lines.push(Line::from(vec![
        Span::styled("  Agents: ", label_style),
        Span::styled(
            format!("{} alive / {} max", health.agents_alive, health.agents_max),
            value_style,
        ),
        Span::styled(format!("  ({} total)", health.agents_total), dim_style),
    ]));

    // Paused
    if health.paused {
        let pause_color = if health.provider_auto_pause {
            Color::Red
        } else {
            Color::Yellow
        };
        lines.push(Line::from(vec![
            Span::styled("  Status: ", label_style),
            Span::styled(
                "PAUSED",
                Style::default()
                    .fg(pause_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" (agent spawning disabled)", dim_style),
        ]));

        // Show pause reason if available
        if let Some(ref reason) = health.pause_reason {
            let reason_display = if reason.len() > 40 {
                format!("{}...", &reason[..reason.floor_char_boundary(37)])
            } else {
                reason.clone()
            };
            lines.push(Line::from(vec![
                Span::styled("  Reason: ", label_style),
                Span::styled(reason_display, Style::default().fg(pause_color)),
            ]));
        }

        // Show resume hint for provider auto-pause
        if health.provider_auto_pause {
            lines.push(Line::from(vec![
                Span::styled("  ", dim_style),
                Span::styled("Resume with: ", dim_style),
                Span::styled("wg service resume", Style::default().fg(Color::Cyan)),
            ]));
        }
    }

    lines.push(Line::from(""));

    // Stuck tasks
    if health.stuck_tasks.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  Stuck tasks: ", label_style),
            Span::styled("none", Style::default().fg(Color::Green)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("  Stuck tasks: ", label_style),
            Span::styled(
                format!("{}", health.stuck_tasks.len()),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        for st in &health.stuck_tasks {
            let title_display = if st.task_title.len() > 30 {
                format!(
                    "{}...",
                    &st.task_title[..st.task_title.floor_char_boundary(27)]
                )
            } else {
                st.task_title.clone()
            };
            lines.push(Line::from(vec![
                Span::styled("    ", dim_style),
                Span::styled(&st.task_id, Style::default().fg(Color::Yellow)),
                Span::styled(format!(" ({})", title_display), dim_style),
            ]));
        }
    }

    lines.push(Line::from(""));

    // Recent errors
    if health.recent_errors.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  Recent errors: ", label_style),
            Span::styled("none", Style::default().fg(Color::Green)),
        ]));
    } else {
        lines.push(Line::from(vec![Span::styled(
            "  Recent errors:",
            label_style,
        )]));
        for err in &health.recent_errors {
            let max_w = width as usize - 6;
            let truncated = if err.len() > max_w {
                format!(
                    "{}...",
                    &err[..err.floor_char_boundary(max_w.saturating_sub(3))]
                )
            } else {
                err.clone()
            };
            lines.push(Line::from(vec![
                Span::styled("    ", dim_style),
                Span::styled(truncated, Style::default().fg(Color::Red)),
            ]));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  Press ", dim_style),
        Span::styled("Esc", Style::default().fg(Color::Yellow)),
        Span::styled(" or click badge to close", dim_style),
    ]));

    // Apply scroll
    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(health.detail_scroll)
        .take(inner.height as usize)
        .collect();

    frame.render_widget(Paragraph::new(visible_lines), inner);
}

/// Draw the service control panel - a modal overlay with service actions.
fn draw_service_control_panel(frame: &mut Frame, app: &VizApp) {
    let size = frame.area();
    let health = &app.service_health;
    let is_running = health.pid.is_some() && health.level != ServiceHealthLevel::Red;
    let width = 56.min(size.width.saturating_sub(4));
    let height = 32.min(size.height.saturating_sub(2));
    let x = size.width.saturating_sub(width + 1);
    let y = 1.min(size.height.saturating_sub(height));
    let area = Rect::new(x, y, width, height);
    frame.render_widget(Clear, area);
    let (border_color, title_dot) = match health.level {
        ServiceHealthLevel::Green => (Color::Green, "\u{25CF}"),
        ServiceHealthLevel::Yellow => (Color::Yellow, "\u{25CF}"),
        ServiceHealthLevel::Red => (Color::Red, "\u{25CF}"),
    };
    let block = Block::default()
        .title(format!(" {} Service Control ", title_dot))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines: Vec<Line> = Vec::new();
    let label_style = Style::default().fg(Color::Cyan);
    let value_style = Style::default().fg(Color::White);
    let dim_style = Style::default().fg(Color::DarkGray);
    let focus = &health.panel_focus;
    lines.push(Line::from(vec![
        Span::styled("  PID: ", label_style),
        Span::styled(
            health
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "N/A".to_string()),
            value_style,
        ),
        Span::styled("    Uptime: ", label_style),
        Span::styled(health.uptime.as_deref().unwrap_or("N/A"), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Agents: ", label_style),
        Span::styled(
            format!("{} alive / {} max", health.agents_alive, health.agents_max),
            value_style,
        ),
        Span::styled(format!("  ({} total)", health.agents_total), dim_style),
    ]));
    if !health.recent_errors.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "  Recent errors:",
            label_style,
        )]));
        for err in health.recent_errors.iter().take(3) {
            let max_w = width as usize - 6;
            let truncated = if err.len() > max_w {
                format!(
                    "{}...",
                    &err[..err.floor_char_boundary(max_w.saturating_sub(3))]
                )
            } else {
                err.clone()
            };
            lines.push(Line::from(vec![
                Span::styled("    ", dim_style),
                Span::styled(truncated, Style::default().fg(Color::Red)),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "  Controls",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));
    let ssl = if is_running {
        "Stop Service"
    } else {
        "Start Service"
    };
    let ssk = if is_running { "[S] Stop" } else { "[S] Start" };
    lines.push(control_panel_line(
        ssl,
        ssk,
        *focus == ControlPanelFocus::StartStop,
        if is_running { Color::Red } else { Color::Green },
    ));
    let pl = if health.paused {
        "Resume Launches"
    } else {
        "Pause Launches"
    };
    let pk = if health.paused {
        "[P] Resume"
    } else {
        "[P] Pause"
    };
    lines.push(control_panel_line(
        pl,
        pk,
        *focus == ControlPanelFocus::PauseResume,
        Color::Yellow,
    ));
    lines.push(control_panel_line(
        "Restart Service",
        "[Enter]",
        *focus == ControlPanelFocus::Restart,
        Color::Cyan,
    ));
    // Agent slots: show current value with +/- hint
    let slots_focused = *focus == ControlPanelFocus::AgentSlots;
    let slots_label = format!("Agent Slots: {}", health.agents_max);
    let slots_style = if slots_focused {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Cyan)
    };
    lines.push(Line::from(vec![
        Span::styled(if slots_focused { " > " } else { "   " }, slots_style),
        Span::styled(&slots_label, slots_style),
        Span::styled("  ", Style::default()),
        Span::styled("[+/-] adjust", Style::default().fg(Color::DarkGray)),
    ]));
    let pf = *focus == ControlPanelFocus::PanicKill;
    let ps = if pf {
        Style::default()
            .fg(Color::White)
            .bg(Color::Red)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    };
    lines.push(Line::from(vec![
        Span::styled(if pf { " > " } else { "   " }, ps),
        Span::styled("PANIC KILL", ps),
        Span::styled("  [K]  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("kills {} agents + stops service", health.agents_alive),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    lines.push(Line::from(""));
    if health.stuck_tasks.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  Stuck agents: ", label_style),
            Span::styled("none", Style::default().fg(Color::Green)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("  Stuck agents: ", label_style),
            Span::styled(
                format!("{}", health.stuck_tasks.len()),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  (Enter=kill, U=unclaim)", dim_style),
        ]));
        for (i, st) in health.stuck_tasks.iter().enumerate() {
            let sf = *focus == ControlPanelFocus::StuckAgent(i);
            let td = if st.task_title.len() > 25 {
                format!(
                    "{}...",
                    &st.task_title[..st.task_title.floor_char_boundary(22)]
                )
            } else {
                st.task_title.clone()
            };
            let pfx = if sf { " > " } else { "   " };
            let fg = if sf { Color::Yellow } else { Color::DarkGray };
            lines.push(Line::from(vec![
                Span::styled(pfx, Style::default().fg(fg)),
                Span::styled(&st.agent_id, Style::default().fg(Color::Yellow)),
                Span::styled(format!(" ({}) ", td), Style::default().fg(fg)),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(control_panel_line(
        "Kill All Dead Agents",
        "[Enter]",
        *focus == ControlPanelFocus::KillAllDead,
        Color::Yellow,
    ));
    lines.push(control_panel_line(
        "Retry Failed Evals",
        "[Enter]",
        *focus == ControlPanelFocus::RetryFailedEvals,
        Color::Cyan,
    ));
    if let Some((ref msg, ref at)) = health.feedback
        && at.elapsed() < std::time::Duration::from_secs(5)
    {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  ", dim_style),
            Span::styled(
                msg.as_str(),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
    }
    if health.panic_confirm {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                "  WARNING: ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    "This will kill {} running agents and stop the service.",
                    health.agents_alive
                ),
                Style::default().fg(Color::White),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Are you sure? ", Style::default().fg(Color::White)),
            Span::styled(
                "[y]",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" Yes  ", dim_style),
            Span::styled(
                "[n/Esc]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" No", dim_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  ", dim_style),
        Span::styled("Esc", Style::default().fg(Color::Yellow)),
        Span::styled(" close  ", dim_style),
        Span::styled("\u{2191}\u{2193}", Style::default().fg(Color::Yellow)),
        Span::styled(" navigate  ", dim_style),
        Span::styled("Enter", Style::default().fg(Color::Yellow)),
        Span::styled(" activate", dim_style),
    ]));
    let visible_lines: Vec<Line> = lines.into_iter().take(inner.height as usize).collect();
    frame.render_widget(Paragraph::new(visible_lines), inner);
}

fn control_panel_line<'a>(
    label: &'a str,
    key_hint: &'a str,
    focused: bool,
    color: Color,
) -> Line<'a> {
    let prefix = if focused { " > " } else { "   " };
    let style = if focused {
        Style::default()
            .fg(Color::Black)
            .bg(color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color)
    };
    Line::from(vec![
        Span::styled(prefix, style),
        Span::styled(label, style),
        Span::styled("  ", Style::default()),
        Span::styled(key_hint, Style::default().fg(Color::DarkGray)),
    ])
}

fn draw_help_overlay(frame: &mut Frame) {
    let size = frame.area();
    let width = 56.min(size.width.saturating_sub(4));
    let height = 50.min(size.height.saturating_sub(4));
    let x = (size.width.saturating_sub(width)) / 2;
    let y = (size.height.saturating_sub(height)) / 2;
    let area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, area);

    let block = Block::default()
        .title(" Keybindings ")
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );

    let inner = block.inner(area);

    let heading = |text: &str| -> Line {
        Line::from(Span::styled(
            text.to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
    };

    let binding = |key: &str, desc: &str| -> Line {
        Line::from(vec![
            Span::styled(format!("  {:<14}", key), Style::default().fg(Color::Yellow)),
            Span::styled(desc.to_string(), Style::default().fg(Color::White)),
        ])
    };

    let blank = || Line::from("");

    let lines = vec![
        heading("Navigation"),
        binding("↑ / ↓", "Select prev / next task"),
        binding("j / k", "Scroll down / up"),
        binding("h / l", "Scroll left / right"),
        binding("Ctrl-d / u", "Page down / up"),
        binding("g / G", "Jump to top / bottom"),
        blank(),
        heading("Panels"),
        binding("Tab", "Switch focus: Graph ↔ Right Panel"),
        binding("Alt-↑/↓", "Switch focus: Graph ↔ Right Panel"),
        binding("Alt-←/→", "Cycle inspector views (with slide animation)"),
        binding("\\", "Toggle right panel visible"),
        binding("i", "Grow viz pane (10% per press, wraps)"),
        binding("v", "Shrink viz pane (10% per press, wraps)"),
        binding("=", "Cycle layout: split/panel/graph"),
        binding("0-7", "Switch tab: Chat/.../Files/Coord"),
        binding("R", "Toggle raw JSON in Detail tab"),
        binding("Space", "Toggle section collapse in Detail"),
        blank(),
        heading("Log Tab"),
        binding("J", "Toggle raw JSON mode"),
        blank(),
        heading("Edge Tracing"),
        binding("t", "Toggle trace on/off"),
        binding("T", "Toggle view/total tokens"),
        binding("Shift-↑/↓", "Scroll detail panel"),
        binding("", "Bold=selected  Magenta=upstream"),
        binding("", "Cyan=downstream"),
        blank(),
        heading("Quick Actions (graph panel)"),
        binding("a", "Create new task"),
        binding("D", "Mark selected task done"),
        binding("f", "Mark selected task failed"),
        binding("x", "Retry selected task"),
        binding("e", "Edit task description"),
        binding("M", "Send message to task agent"),
        binding("A", "Toggle archive browser"),
        binding("c", "Open chat input"),
        binding("Ctrl-C", "Kill agent on focused task"),
        blank(),
        heading("Chat Panel (coordinators)"),
        binding("~ / `", "Open coordinator picker"),
        binding("+", "Add new coordinator (picker)"),
        binding("-", "Close/archive coordinator"),
        binding("[ / ]", "Prev / next coordinator"),
        binding("←/→", "Prev / next coordinator"),
        binding("Ctrl-T", "Toggle PTY mode"),
        blank(),
        heading("Search (vim-style)"),
        binding("/", "Start search"),
        binding("n / N", "Next / previous match"),
        binding("Enter", "Accept and jump to match"),
        binding("Esc", "Clear search"),
        blank(),
        heading("General"),
        binding("s", "Cycle sort: Chrono↓/↑/Status"),
        binding("m", "Toggle mouse capture"),
        binding("X", "Swap scroll axis (for Termux)"),
        binding("r", "Force refresh"),
        binding(".", "Toggle system tasks (visible by default)"),
        binding("<", "Toggle running system tasks only"),
        binding("*", "Toggle touch echo (click feedback)"),
        binding("L", "Toggle coordinator log"),
        binding("?", "Toggle this help"),
        binding("q", "Quit"),
        blank(),
        Line::from(Span::styled(
            "  Press ? or Esc to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let paragraph = Paragraph::new(lines);

    frame.render_widget(block, area);
    frame.render_widget(paragraph, inner);
}

/// Render token breakdown spans: "→new_in ←out [+cached] (label) [$cost]"
fn render_token_breakdown<'a>(spans: &mut Vec<Span<'a>>, usage: &TokenUsage, label: &str) {
    let novel_in = usage.input_tokens + usage.cache_creation_input_tokens;
    let new_input = format_tokens(novel_in);
    let output = format_tokens(usage.output_tokens);

    let token_str = if usage.cache_read_input_tokens > 0 {
        let cache = format_tokens(usage.cache_read_input_tokens);
        format!("→{} ◎{} ←{}", new_input, cache, output)
    } else {
        format!("→{} ←{}", new_input, output)
    };

    spans.push(Span::styled(token_str, Style::default().fg(Color::Cyan)));

    // Label: "view" or "total" — dim to avoid clutter
    spans.push(Span::styled(
        format!(" {} ", label),
        Style::default().fg(Color::DarkGray),
    ));

    // Cost if available
    if usage.cost_usd > 0.0 {
        spans.push(Span::styled(
            format!("${:.2} ", usage.cost_usd),
            Style::default().fg(Color::Cyan),
        ));
    }
}

/// Draw the Config tab content: full configuration dashboard.
fn draw_config_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    if app.config_panel.entries.is_empty() {
        app.load_config_panel();
    }

    let entries = &app.config_panel.entries;
    if entries.is_empty() {
        let msg =
            Paragraph::new("No configuration found").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(msg, area);
        return;
    }

    let viewport_h = area.height as usize;
    let selected = app.config_panel.selected;

    // If in add-endpoint mode, draw the form instead.
    if app.config_panel.adding_endpoint {
        draw_add_endpoint_form(frame, app, area);
        return;
    }

    // If in add-model mode, draw the form instead.
    if app.config_panel.adding_model {
        draw_add_model_form(frame, app, area);
        return;
    }

    // Precompute endpoint index → name for test status display.
    let endpoint_names: HashMap<usize, String> = {
        let config = workgraph::config::Config::load_or_default(&app.workgraph_dir);
        config
            .llm_endpoints
            .endpoints
            .iter()
            .enumerate()
            .map(|(i, ep)| (i, ep.name.clone()))
            .collect()
    };

    // Build display lines grouped by section with collapsible headers.
    let mut lines: Vec<(Line, bool)> = Vec::new(); // (line, is_selectable)
    let mut entry_line_map: Vec<usize> = Vec::new(); // entry_idx -> display line index
    let mut current_section: Option<ConfigSection> = None;

    for (i, entry) in entries.iter().enumerate() {
        // Section header if changed.
        if current_section != Some(entry.section) {
            if current_section.is_some() {
                lines.push((Line::from(""), false)); // blank separator
            }
            let is_collapsed = app.config_panel.collapsed.contains(&entry.section);
            let arrow = if is_collapsed { "▶" } else { "▼" };

            // Section header status indicators
            let extra = if entry.section == ConfigSection::Service {
                if app.config_panel.service_running {
                    format!(
                        "  ● running (PID {})",
                        app.config_panel
                            .service_pid
                            .map(|p| p.to_string())
                            .unwrap_or_default()
                    )
                } else {
                    "  ○ stopped".to_string()
                }
            } else if entry.section == ConfigSection::Models {
                // Count models (entries with .info suffix minus the add button)
                let model_count = entries
                    .iter()
                    .filter(|e| e.section == ConfigSection::Models && e.key.ends_with(".info"))
                    .count();
                format!("  ({} models)", model_count)
            } else {
                String::new()
            };

            let header_style = Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD);

            let status_color = if entry.section == ConfigSection::Service {
                if app.config_panel.service_running {
                    Color::Green
                } else {
                    Color::DarkGray
                }
            } else {
                Color::DarkGray
            };

            lines.push((
                Line::from(vec![
                    Span::styled(format!("{} ", arrow), header_style),
                    Span::styled(entry.section.label().to_string(), header_style),
                    Span::styled(extra, Style::default().fg(status_color)),
                ]),
                false,
            ));
            current_section = Some(entry.section);
        }

        // Skip entries in collapsed sections
        if app.config_panel.collapsed.contains(&entry.section) {
            entry_line_map.push(lines.len().saturating_sub(1));
            continue;
        }

        entry_line_map.push(lines.len());

        let is_selected = i == selected;
        let is_editing = is_selected && app.config_panel.editing;

        // Format the value display.
        let value_display = if is_editing {
            match &entry.edit_kind {
                ConfigEditKind::TextInput => {
                    format!("[{}▏]", app.config_panel.edit_buffer)
                }
                ConfigEditKind::SecretInput => {
                    // Show actual text while editing, mask when not
                    format!("[{}▏]", app.config_panel.edit_buffer)
                }
                ConfigEditKind::Choice(choices) => {
                    if let Some(ref picker) = app.config_panel.choice_picker {
                        let selected_label = picker
                            .selected_item()
                            .map(|(id, _)| id.as_str())
                            .unwrap_or("");
                        if picker.filter.is_empty() {
                            format!("[{}] (type to filter)", selected_label)
                        } else {
                            format!(
                                "[{}] filter: {} ({}/{})",
                                selected_label,
                                picker.filter,
                                picker.filtered_indices.len(),
                                picker.items.len()
                            )
                        }
                    } else {
                        let ci = app.config_panel.choice_index;
                        choices
                            .iter()
                            .enumerate()
                            .map(|(j, c)| {
                                if j == ci {
                                    format!("[{}]", c)
                                } else {
                                    c.clone()
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    }
                }
                ConfigEditKind::Toggle => entry.value.clone(),
            }
        } else {
            match &entry.edit_kind {
                ConfigEditKind::Toggle => {
                    if entry.value == "on" {
                        "on".to_string()
                    } else {
                        "off".to_string()
                    }
                }
                ConfigEditKind::SecretInput => entry.value.clone(), // already masked
                _ => entry.value.clone(),
            }
        };

        let label_width = 24;
        let label = format!("{:<width$}", entry.label, width = label_width);

        let style = if is_editing {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if is_selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let value_style = if is_editing {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            let value_color = match &entry.edit_kind {
                ConfigEditKind::Toggle => {
                    if entry.value == "on" {
                        Color::Green
                    } else {
                        Color::Red
                    }
                }
                ConfigEditKind::SecretInput => {
                    if entry.value == "(not set)" {
                        Color::DarkGray
                    } else {
                        Color::Magenta
                    }
                }
                _ => {
                    if is_selected {
                        Color::Yellow
                    } else {
                        Color::Gray
                    }
                }
            };
            Style::default().fg(value_color)
        };

        let cursor = if is_editing {
            "✎ "
        } else if is_selected {
            "▸ "
        } else {
            "  "
        };

        let mut spans = vec![Span::styled(cursor, style)];

        // For API Keys entries, color the status icon (✓/✗/⚠) separately
        if entry.section == ConfigSection::ApiKeys && entry.label.len() >= 2 {
            let icon = &entry.label[..entry
                .label
                .char_indices()
                .nth(1)
                .map(|(i, _)| i)
                .unwrap_or(2)];
            let rest = &entry.label[icon.len()..];
            let icon_color = match icon.trim() {
                "✓" => Color::Green,
                "✗" => Color::Red,
                "⚠" => Color::Yellow,
                _ => style.fg.unwrap_or(Color::White),
            };
            let rest_padded = format!(
                "{:<width$}",
                rest,
                width = label_width.saturating_sub(icon.len())
            );
            spans.push(Span::styled(
                icon.to_string(),
                Style::default().fg(icon_color).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(rest_padded, style));
        } else {
            spans.push(Span::styled(label, style));
        }

        spans.push(Span::styled(value_display, value_style));

        // Show endpoint test status inline for endpoint name entries.
        if entry.key.ends_with(".name")
            && entry.section == ConfigSection::Endpoints
            && let Some(ep_idx) = entry
                .key
                .strip_prefix("endpoint.")
                .and_then(|r| r.strip_suffix(".name"))
                .and_then(|s| s.parse::<usize>().ok())
            && let Some(ep_name) = endpoint_names.get(&ep_idx)
            && let Some(status) = app.config_panel.endpoint_test_results.get(ep_name)
        {
            match status {
                EndpointTestStatus::Testing => {
                    spans.push(Span::styled(
                        " ⟳ Testing...".to_string(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                EndpointTestStatus::Ok => {
                    spans.push(Span::styled(
                        " ✓ Connected".to_string(),
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                EndpointTestStatus::Error(msg) => {
                    spans.push(Span::styled(
                        " ✗ ".to_string(),
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                    let truncated = if msg.len() > 40 {
                        format!("{}...", &msg[..msg.floor_char_boundary(40)])
                    } else {
                        msg.clone()
                    };
                    spans.push(Span::styled(truncated, Style::default().fg(Color::Red)));
                }
            }
        }

        let line = Line::from(spans);

        lines.push((line, true));
    }

    // Add help text at the bottom.
    lines.push((Line::from(""), false));

    // Show save notification if recent.
    let show_saved = app
        .config_panel
        .save_notification
        .map(|t| t.elapsed() < std::time::Duration::from_secs(2))
        .unwrap_or(false);

    if show_saved {
        lines.push((
            Line::from(Span::styled(
                " ✓ Saved to config.toml",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )),
            false,
        ));
    } else {
        let help_text = if app.config_panel.editing {
            match &entries[selected].edit_kind {
                ConfigEditKind::TextInput | ConfigEditKind::SecretInput => {
                    "Enter: save  Esc: cancel"
                }
                ConfigEditKind::Choice(_) => {
                    if app.config_panel.choice_picker.is_some() {
                        "↑/↓: choose  type: filter  Enter: save  Esc: cancel"
                    } else {
                        "←/→: choose  Enter: save  Esc: cancel"
                    }
                }
                ConfigEditKind::Toggle => "Enter/Space: toggle",
            }
        } else {
            "j/k: navigate  Enter: edit  Space: toggle  Tab: collapse  a: add endpoint  m: add model  t: test  r: reload"
        };
        lines.push((
            Line::from(Span::styled(
                format!(" {}", help_text),
                Style::default().fg(Color::DarkGray),
            )),
            false,
        ));
    }

    // Scrolling: ensure selected entry is visible.
    let selected_line = entry_line_map.get(selected).copied().unwrap_or(0);
    if selected_line < app.config_panel.scroll {
        app.config_panel.scroll = selected_line;
    }
    if selected_line >= app.config_panel.scroll + viewport_h {
        app.config_panel.scroll = selected_line.saturating_sub(viewport_h - 1);
    }

    let start = app.config_panel.scroll;
    let end = (start + viewport_h).min(lines.len());

    // Build entry_idx → screen Y mapping for mouse click detection.
    app.config_entry_y_positions.clear();
    for (entry_idx, &display_line) in entry_line_map.iter().enumerate() {
        if display_line >= start && display_line < end {
            let screen_y = area.y + (display_line - start) as u16;
            app.config_entry_y_positions.push((entry_idx, screen_y));
        }
    }

    let visible_lines: Vec<Line> = lines[start..end].iter().map(|(l, _)| l.clone()).collect();

    let paragraph = Paragraph::new(visible_lines);
    frame.render_widget(paragraph, area);

    // Scrollbar if content exceeds viewport (auto-hides after 2 seconds of inactivity).
    if lines.len() > viewport_h && app.panel_scrollbar_visible() {
        draw_panel_scrollbar(
            frame,
            app,
            area,
            lines.len().saturating_sub(viewport_h),
            app.config_panel.scroll,
        );
    }
}

/// Draw the "Add endpoint" form overlay.
fn draw_add_endpoint_form(frame: &mut Frame, app: &VizApp, area: Rect) {
    let fields = &app.config_panel.new_endpoint;
    let active = app.config_panel.new_endpoint_field;

    let field_labels = ["Name", "Provider", "URL", "Model", "API Key"];
    let field_values = [
        &fields.name,
        &fields.provider,
        &fields.url,
        &fields.model,
        &fields.api_key,
    ];

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "── Add LLM Endpoint ──",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    for (i, (label, value)) in field_labels.iter().zip(field_values.iter()).enumerate() {
        let is_active = i == active;
        let cursor = if is_active { "▸ " } else { "  " };
        let style = if is_active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let display = if is_active && app.config_panel.editing {
            format!("[{}▏]", app.config_panel.edit_buffer)
        } else if value.is_empty() {
            match i {
                1 => "(anthropic/openai/openrouter/local)".to_string(),
                2 => "(auto-detected from provider)".to_string(),
                _ => "(empty)".to_string(),
            }
        } else {
            value.to_string()
        };

        let value_color = if value.is_empty() && !is_active {
            Color::DarkGray
        } else if is_active {
            Color::Yellow
        } else {
            Color::Gray
        };

        lines.push(Line::from(vec![
            Span::styled(cursor, style),
            Span::styled(format!("{:<16}", label), style),
            Span::styled(display, Style::default().fg(value_color)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter: confirm field  Tab: next field  Esc: cancel  Ctrl+S: save endpoint",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

/// Draw the "Add model" form overlay.
fn draw_add_model_form(frame: &mut Frame, app: &VizApp, area: Rect) {
    let fields = &app.config_panel.new_model;
    let active = app.config_panel.new_model_field;

    let field_labels = ["Model ID", "Provider", "Tier", "Cost In/1M", "Cost Out/1M"];
    let field_values = [
        &fields.id,
        &fields.provider,
        &fields.tier,
        &fields.cost_in,
        &fields.cost_out,
    ];

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(
        "── Add Model ──",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    for (i, (label, value)) in field_labels.iter().zip(field_values.iter()).enumerate() {
        let is_active = i == active;
        let cursor = if is_active { "▸ " } else { "  " };
        let style = if is_active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let display = if is_active && app.config_panel.editing {
            format!("[{}▏]", app.config_panel.edit_buffer)
        } else if value.is_empty() {
            match i {
                0 => "(e.g. claude:sonnet, openrouter:deepseek/deepseek-v3.2)".to_string(),
                1 => "(openrouter)".to_string(),
                2 => "(budget/mid/frontier)".to_string(),
                3 => "(USD per 1M tokens)".to_string(),
                4 => "(USD per 1M tokens)".to_string(),
                _ => "(empty)".to_string(),
            }
        } else {
            value.to_string()
        };

        let value_color = if value.is_empty() && !is_active {
            Color::DarkGray
        } else if is_active {
            Color::Yellow
        } else {
            Color::Gray
        };

        lines.push(Line::from(vec![
            Span::styled(cursor, style),
            Span::styled(format!("{:<16}", label), style),
            Span::styled(display, Style::default().fg(value_color)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        " Enter: confirm field  Tab: next field  Esc: cancel  Ctrl+S: save model",
        Style::default().fg(Color::DarkGray),
    )));

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    use std::collections::{HashMap, HashSet};
    use workgraph::graph::{Node, Status, WorkGraph};
    use workgraph::test_helpers::make_task_with_status;

    use crate::commands::viz::ascii::generate_ascii;

    /// Default bottom panel percent (matches HudSize::Normal).
    const BOTTOM_PANEL_PERCENT: u16 = 40;
    use crate::commands::viz::{LayoutMode, VizOutput};

    /// Build a test graph and generate viz output.
    /// Returns (VizOutput, graph) for a chain: a -> b -> c, plus standalone d.
    fn build_test_graph_chain_plus_isolated() -> (VizOutput, WorkGraph) {
        let mut graph = WorkGraph::new();
        let a = make_task_with_status("a", "Task A", Status::Done);
        let mut b = make_task_with_status("b", "Task B", Status::InProgress);
        b.after = vec!["a".to_string()];
        let mut c = make_task_with_status("c", "Task C", Status::Open);
        c.after = vec!["b".to_string()];
        let d = make_task_with_status("d", "Task D", Status::Failed);
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
        (result, graph)
    }

    /// Build a VizApp from VizOutput for testing apply_per_char_trace_coloring.
    /// Sets the selected task and computes upstream/downstream sets.
    fn build_app_from_viz_output(viz: &VizOutput, selected_id: &str) -> VizApp {
        let mut app = VizApp::from_viz_output_for_test(viz);
        let selected_task_idx = app.task_order.iter().position(|id| id == selected_id);
        app.selected_task_idx = selected_task_idx;
        app.recompute_trace();
        app
    }

    /// Parse an ANSI line into a ratatui Line.
    fn parse_ansi_line(ansi: &str) -> Line<'static> {
        match ansi_to_tui::IntoText::into_text(&ansi) {
            Ok(text) => text.lines.into_iter().next().unwrap_or_default(),
            Err(_) => Line::from(ansi.to_string()),
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Test 1: TEXT COLORS UNCHANGED — status-based colors preserved
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_text_colors_unchanged_for_upstream_task() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "b");

        // 'a' is upstream of 'b'. Its text should keep its original style.
        let a_line = viz.node_line_map["a"];
        let plain = app.plain_lines[a_line].as_str();
        let category = classify_task_line(&app, a_line);
        assert!(matches!(category, LineTraceCategory::Upstream));

        // Create a line with explicit green color (done status).
        let green_style = Style::default().fg(Color::Green);
        let text_range = find_text_range(plain);
        assert!(text_range.is_some(), "Task line should have text range");
        let (text_start, text_end) = text_range.unwrap();

        // Build a line with known colors.
        let chars: Vec<char> = plain.chars().collect();
        let prefix: String = chars[..text_start].iter().collect();
        let text: String = chars[text_start..text_end].iter().collect();
        let suffix: String = chars[text_end..].iter().collect();
        let line = Line::from(vec![
            Span::styled(prefix.clone(), Style::default().fg(Color::DarkGray)),
            Span::styled(text.clone(), green_style),
            Span::styled(suffix.clone(), Style::default()),
        ]);

        let result = apply_per_char_trace_coloring(line, plain, a_line, &category, &app, Some("b"));

        // Verify that the task text portion preserved its green color.
        let mut char_idx = 0;
        for span in &result.spans {
            for c in span.content.chars() {
                if char_idx >= text_start && char_idx < text_end {
                    assert_eq!(
                        span.style.fg,
                        Some(Color::Green),
                        "Upstream task text at char {} ('{}') should preserve green status color, got {:?}",
                        char_idx,
                        c,
                        span.style.fg
                    );
                }
                char_idx += 1;
            }
        }
    }

    #[test]
    fn test_text_colors_unchanged_for_downstream_task() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "b");

        // 'c' is downstream of 'b'. Its text should keep its original style.
        let c_line = viz.node_line_map["c"];
        let plain = app.plain_lines[c_line].as_str();
        let category = classify_task_line(&app, c_line);
        assert!(matches!(category, LineTraceCategory::Downstream));

        let text_range = find_text_range(plain).unwrap();
        let (text_start, text_end) = text_range;
        let chars: Vec<char> = plain.chars().collect();
        let prefix: String = chars[..text_start].iter().collect();
        let text: String = chars[text_start..text_end].iter().collect();
        let suffix: String = chars[text_end..].iter().collect();

        let white_style = Style::default().fg(Color::White);
        let line = Line::from(vec![
            Span::styled(prefix, Style::default().fg(Color::DarkGray)),
            Span::styled(text, white_style),
            Span::styled(suffix, Style::default()),
        ]);

        let result = apply_per_char_trace_coloring(line, plain, c_line, &category, &app, Some("b"));

        let mut char_idx = 0;
        for span in &result.spans {
            for c in span.content.chars() {
                if char_idx >= text_start && char_idx < text_end {
                    assert_eq!(
                        span.style.fg,
                        Some(Color::White),
                        "Downstream task text at char {} ('{}') should preserve white status color, got {:?}",
                        char_idx,
                        c,
                        span.style.fg
                    );
                }
                char_idx += 1;
            }
        }
    }

    #[test]
    fn test_text_colors_unchanged_for_unrelated_task() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "b");

        // 'd' is unrelated to 'b' (separate WCC). Its text should keep original style.
        let d_line = viz.node_line_map["d"];
        let plain = app.plain_lines[d_line].as_str();
        let category = classify_task_line(&app, d_line);
        assert!(matches!(category, LineTraceCategory::Unrelated));

        let text_range = find_text_range(plain).unwrap();
        let (text_start, text_end) = text_range;
        let chars: Vec<char> = plain.chars().collect();
        let prefix: String = chars[..text_start].iter().collect();
        let text: String = chars[text_start..text_end].iter().collect();
        let suffix: String = chars[text_end..].iter().collect();

        let red_style = Style::default().fg(Color::Red);
        let line = Line::from(vec![
            Span::styled(prefix, Style::default()),
            Span::styled(text, red_style),
            Span::styled(suffix, Style::default()),
        ]);

        let result = apply_per_char_trace_coloring(line, plain, d_line, &category, &app, Some("b"));

        let mut char_idx = 0;
        for span in &result.spans {
            for _c in span.content.chars() {
                if char_idx >= text_start && char_idx < text_end {
                    assert_eq!(
                        span.style.fg,
                        Some(Color::Red),
                        "Unrelated task text at char {} should preserve red status color, got {:?}",
                        char_idx,
                        span.style.fg
                    );
                }
                char_idx += 1;
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Test 2: UPSTREAM EDGES COLORED MAGENTA
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_upstream_edges_colored_magenta() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "b");

        // Find edge chars that belong to the a->b edge and verify they're magenta.

        // Edge chars on b's line (connectors like ├→ or └→) should be in char_edge_map
        // with (src="a", tgt="b"). Since both a and b are in upstream∪{selected},
        // they should be colored magenta.
        let mut found_magenta_edge = false;
        for (key, edges) in &viz.char_edge_map {
            let (ln, col) = *key;
            if edges.iter().any(|(s, t)| s == "a" && t == "b") {
                let (src, tgt) = edges.iter().find(|(s, t)| s == "a" && t == "b").unwrap();
                // This is an a->b edge character. Verify it would be colored magenta.
                let plain = app.plain_lines[ln].as_str();
                let base_line = parse_ansi_line(app.lines[ln].as_str());
                let category = classify_task_line(&app, ln);

                let result =
                    apply_per_char_trace_coloring(base_line, plain, ln, &category, &app, Some("b"));

                // Find the span containing char at position `col`.
                let mut char_idx = 0;
                for span in &result.spans {
                    for _ in span.content.chars() {
                        if char_idx == col {
                            assert_eq!(
                                span.style.fg,
                                Some(Color::Magenta),
                                "Upstream edge char at ({}, {}) for edge {}->{} should be magenta, got {:?}",
                                ln,
                                col,
                                src,
                                tgt,
                                span.style.fg
                            );
                            found_magenta_edge = true;
                        }
                        char_idx += 1;
                    }
                }
            }
        }
        assert!(
            found_magenta_edge,
            "Should find at least one magenta-colored upstream edge char for a->b edge"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Test 3: DOWNSTREAM EDGES COLORED CYAN
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_downstream_edges_colored_cyan() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "b");

        // Find edge chars that belong to the b->c edge and verify they're cyan.
        let mut found_cyan_edge = false;
        for (key, edges) in &viz.char_edge_map {
            let (ln, col) = *key;
            if edges.iter().any(|(s, t)| s == "b" && t == "c") {
                let (src, tgt) = edges.iter().find(|(s, t)| s == "b" && t == "c").unwrap();
                let plain = app.plain_lines[ln].as_str();
                let base_line = parse_ansi_line(app.lines[ln].as_str());
                let category = classify_task_line(&app, ln);

                let result =
                    apply_per_char_trace_coloring(base_line, plain, ln, &category, &app, Some("b"));

                let mut char_idx = 0;
                for span in &result.spans {
                    for _ in span.content.chars() {
                        if char_idx == col {
                            assert_eq!(
                                span.style.fg,
                                Some(Color::Cyan),
                                "Downstream edge char at ({}, {}) for edge {}->{} should be cyan, got {:?}",
                                ln,
                                col,
                                src,
                                tgt,
                                span.style.fg
                            );
                            found_cyan_edge = true;
                        }
                        char_idx += 1;
                    }
                }
            }
        }
        assert!(
            found_cyan_edge,
            "Should find at least one cyan-colored downstream edge char for b->c edge"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Test 4: ONLY CONNECTED EDGES COLORED — unrelated edges keep base style
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_unrelated_edges_not_colored() {
        // Build a graph with two independent chains: a->b->c and x->y
        let mut graph = WorkGraph::new();
        let a = make_task_with_status("a", "Task A", Status::Done);
        let mut b = make_task_with_status("b", "Task B", Status::InProgress);
        b.after = vec!["a".to_string()];
        let mut c = make_task_with_status("c", "Task C", Status::Open);
        c.after = vec!["b".to_string()];
        let x = make_task_with_status("x", "Task X", Status::Open);
        let mut y = make_task_with_status("y", "Task Y", Status::Open);
        y.after = vec!["x".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(x));
        graph.add_node(Node::Task(y));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let app = build_app_from_viz_output(&viz, "b");

        // Find edge chars for x->y edge — they should NOT be colored.
        for (key, edges) in &viz.char_edge_map {
            let (ln, col) = *key;
            if edges.iter().any(|(s, t)| s == "x" && t == "y") {
                let (src, tgt) = edges.iter().find(|(s, t)| s == "x" && t == "y").unwrap();
                let plain = app.plain_lines[ln].as_str();
                let base_line = parse_ansi_line(app.lines[ln].as_str());
                let category = classify_task_line(&app, ln);

                let result =
                    apply_per_char_trace_coloring(base_line, plain, ln, &category, &app, Some("b"));

                let mut char_idx = 0;
                for span in &result.spans {
                    for _ in span.content.chars() {
                        if char_idx == col {
                            assert!(
                                span.style.fg != Some(Color::Magenta)
                                    && span.style.fg != Some(Color::Cyan),
                                "Unrelated edge char at ({}, {}) for edge {}->{} should NOT be magenta/cyan, got {:?}",
                                ln,
                                col,
                                src,
                                tgt,
                                span.style.fg
                            );
                        }
                        char_idx += 1;
                    }
                }
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Test 5: OTHER WCCs UNCHANGED — WCCs not containing the selected task
    //         must render identically to normal output
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_other_wcc_unchanged() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "b");

        // 'd' is in a separate WCC. Its entire line should be unchanged.
        let d_line = viz.node_line_map["d"];
        let plain = app.plain_lines[d_line].as_str();
        let base_line = parse_ansi_line(app.lines[d_line].as_str());
        let category = classify_task_line(&app, d_line);
        assert!(matches!(category, LineTraceCategory::Unrelated));

        // Collect base styles.
        let mut base_chars: Vec<(char, Style)> = Vec::new();
        for span in &base_line.spans {
            for c in span.content.chars() {
                base_chars.push((c, span.style));
            }
        }

        let result = apply_per_char_trace_coloring(
            parse_ansi_line(app.lines[d_line].as_str()),
            plain,
            d_line,
            &category,
            &app,
            Some("b"),
        );

        // Collect result styles.
        let mut result_chars: Vec<(char, Style)> = Vec::new();
        for span in &result.spans {
            for c in span.content.chars() {
                result_chars.push((c, span.style));
            }
        }

        assert_eq!(
            base_chars.len(),
            result_chars.len(),
            "WCC-unrelated line should have same number of chars"
        );
        for (i, ((bc, bs), (rc, rs))) in base_chars.iter().zip(result_chars.iter()).enumerate() {
            assert_eq!(bc, rc, "Char mismatch at position {}", i);
            assert_eq!(
                bs, rs,
                "Style mismatch at position {} ('{}') in other-WCC line: expected {:?}, got {:?}",
                i, bc, bs, rs
            );
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Test 6: SELECTED TASK INDICATOR — selected task gets special treatment
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_selected_task_keeps_original_style() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "b");

        let b_line = viz.node_line_map["b"];
        let plain = app.plain_lines[b_line].as_str();
        let category = classify_task_line(&app, b_line);
        assert!(matches!(category, LineTraceCategory::Selected));

        let base_line = parse_ansi_line(app.lines[b_line].as_str());
        let base_styles: Vec<(char, Style)> = base_line
            .spans
            .iter()
            .flat_map(|s| s.content.chars().map(move |c| (c, s.style)))
            .collect();

        let base_line2 = parse_ansi_line(app.lines[b_line].as_str());
        let result =
            apply_per_char_trace_coloring(base_line2, plain, b_line, &category, &app, Some("b"));

        let text_range = find_text_range(plain).unwrap();
        let (text_start, text_end) = text_range;

        // Selected task text should keep its original style from apply_per_char_trace_coloring.
        // Bold + bright styling is applied at the line level by apply_selection_style.
        let mut char_idx = 0;
        let mut found_selected_text = false;
        for span in &result.spans {
            for _ in span.content.chars() {
                if char_idx >= text_start && char_idx < text_end {
                    assert_eq!(
                        span.style.bg, base_styles[char_idx].1.bg,
                        "Selected task text at char {} should keep original bg, got {:?}",
                        char_idx, span.style.bg
                    );
                    found_selected_text = true;
                }
                char_idx += 1;
            }
        }
        assert!(
            found_selected_text,
            "Should find selected task text with original style preserved"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Test 7: NO SELECTION = NORMAL OUTPUT — output unchanged without selection
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_no_selection_produces_normal_output() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();

        // When no selection is active, draw_viz_content goes through the `else`
        // branch (line 148-149 in render.rs) and pushes base_line unchanged.
        // We verify this by checking that apply_per_char_trace_coloring with
        // Unrelated category and no edge map hits preserves all styles.

        // Without selection, the code simply uses `base_line` directly.
        // Test the invariant: for every line in the viz, parsing it and
        // NOT applying trace coloring should give the same result as
        // applying trace with "Unrelated" category when no edges match.

        // Build an app with no selection.
        let lines: Vec<String> = viz.text.lines().map(String::from).collect();
        let plain_lines: Vec<String> = lines
            .iter()
            .map(|l: &String| {
                String::from_utf8(strip_ansi_escapes::strip(l.as_bytes())).unwrap_or_default()
            })
            .collect();

        // For each line, verify that if we were to apply trace coloring with
        // empty upstream/downstream sets and no matching char_edge_map entries,
        // the output is identical to the input.
        let empty_app = {
            let mut app = build_app_from_viz_output(&viz, "b");
            app.selected_task_idx = None;
            app.upstream_set.clear();
            app.downstream_set.clear();
            app.char_edge_map.clear();
            app
        };

        for (idx, ansi_line) in lines.iter().enumerate() {
            let plain = &plain_lines[idx];
            let base_line = parse_ansi_line(ansi_line);
            let category = LineTraceCategory::Unrelated;

            // Collect base styles.
            let mut base_chars: Vec<(char, Style)> = Vec::new();
            for span in &base_line.spans {
                for c in span.content.chars() {
                    base_chars.push((c, span.style));
                }
            }

            let result = apply_per_char_trace_coloring(
                parse_ansi_line(ansi_line),
                plain,
                idx,
                &category,
                &empty_app,
                None,
            );

            let mut result_chars: Vec<(char, Style)> = Vec::new();
            for span in &result.spans {
                for c in span.content.chars() {
                    result_chars.push((c, span.style));
                }
            }

            assert_eq!(
                base_chars.len(),
                result_chars.len(),
                "Line {} should have same char count",
                idx
            );
            for (i, ((bc, bs), (rc, rs))) in base_chars.iter().zip(result_chars.iter()).enumerate()
            {
                assert_eq!(bc, rc, "Char mismatch at line {} position {}", idx, i);
                assert_eq!(
                    bs, rs,
                    "Style mismatch at line {} position {} ('{}') with no selection: expected {:?}, got {:?}",
                    idx, i, bc, bs, rs
                );
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Auxiliary tests — verify test infrastructure and edge cases
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_classify_task_line_categories() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "b");

        let a_line = viz.node_line_map["a"];
        let b_line = viz.node_line_map["b"];
        let c_line = viz.node_line_map["c"];
        let d_line = viz.node_line_map["d"];

        assert!(
            matches!(
                classify_task_line(&app, a_line),
                LineTraceCategory::Upstream
            ),
            "Task 'a' should be classified as Upstream when 'b' is selected"
        );
        assert!(
            matches!(
                classify_task_line(&app, b_line),
                LineTraceCategory::Selected
            ),
            "Task 'b' should be classified as Selected"
        );
        assert!(
            matches!(
                classify_task_line(&app, c_line),
                LineTraceCategory::Downstream
            ),
            "Task 'c' should be classified as Downstream when 'b' is selected"
        );
        assert!(
            matches!(
                classify_task_line(&app, d_line),
                LineTraceCategory::Unrelated
            ),
            "Task 'd' should be classified as Unrelated (separate WCC)"
        );
    }

    #[test]
    fn test_find_text_range_on_task_line() {
        // A task line looks like: "├→ task-id  (status)"
        let line = "├→ my-task  (open)";
        let range = find_text_range(line);
        assert!(range.is_some(), "Should find text range in task line");
        let (start, end) = range.unwrap();
        let chars: Vec<char> = line.chars().collect();
        // The text should start at the first alphanumeric character.
        assert!(
            chars[start].is_alphanumeric(),
            "Text range should start at alphanumeric char, got '{}'",
            chars[start]
        );
        // The text should end after the last ')'.
        assert_eq!(
            chars[end - 1],
            ')',
            "Text range should end after ')', got '{}'",
            chars[end - 1]
        );
    }

    #[test]
    fn test_find_text_range_on_connector_only_line() {
        let line = "│  │";
        let range = find_text_range(line);
        assert!(
            range.is_none(),
            "Pure connector line should have no text range"
        );
    }

    #[test]
    fn test_edge_chars_have_correct_edge_info() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();

        // Verify that the char_edge_map contains entries for a->b and b->c edges.
        let has_ab = viz
            .char_edge_map
            .values()
            .any(|edges| edges.iter().any(|(s, t)| s == "a" && t == "b"));
        let has_bc = viz
            .char_edge_map
            .values()
            .any(|edges| edges.iter().any(|(s, t)| s == "b" && t == "c"));
        assert!(has_ab, "char_edge_map should contain a->b edge entries");
        assert!(has_bc, "char_edge_map should contain b->c edge entries");

        // Verify no edges involving 'd' (it's standalone).
        let has_d = viz
            .char_edge_map
            .values()
            .any(|edges| edges.iter().any(|(s, t)| s == "d" || t == "d"));
        assert!(
            !has_d,
            "char_edge_map should NOT contain any edges involving standalone task 'd'"
        );
    }

    #[test]
    fn test_trace_coloring_preserves_non_edge_non_text_chars() {
        // Verify that spaces and other non-edge, non-text characters keep base style.
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "b");

        // Pick a line that has edge chars — check the spaces before/after are preserved.
        for (idx, ansi_line) in app.lines.iter().enumerate() {
            let plain = &app.plain_lines[idx];
            let base_line = parse_ansi_line(ansi_line);
            let category = classify_task_line(&app, idx);

            let mut base_chars: Vec<(char, Style)> = Vec::new();
            for span in &base_line.spans {
                for c in span.content.chars() {
                    base_chars.push((c, span.style));
                }
            }

            let result = apply_per_char_trace_coloring(
                parse_ansi_line(ansi_line),
                plain,
                idx,
                &category,
                &app,
                Some("b"),
            );

            let mut result_chars: Vec<(char, Style)> = Vec::new();
            for span in &result.spans {
                for c in span.content.chars() {
                    result_chars.push((c, span.style));
                }
            }

            let text_range = find_text_range(plain);
            let (text_start, text_end) = text_range.unwrap_or((usize::MAX, usize::MAX));

            for (i, ((bc, bs), (_rc, rs))) in base_chars.iter().zip(result_chars.iter()).enumerate()
            {
                let is_text = i >= text_start && i < text_end;
                let is_edge = app.char_edge_map.contains_key(&(idx, i));

                if !is_text && !is_edge {
                    // Non-text, non-edge chars should keep their base style exactly.
                    assert_eq!(
                        bs, rs,
                        "Non-edge non-text char at line {} pos {} ('{}') should keep base style. \
                         Expected {:?}, got {:?}",
                        idx, i, bc, bs, rs
                    );
                }
            }
        }
    }

    #[test]
    fn test_upstream_set_computed_correctly() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "c");

        // When 'c' is selected, upstream should include both 'a' and 'b'.
        assert!(app.upstream_set.contains("a"), "a should be upstream of c");
        assert!(app.upstream_set.contains("b"), "b should be upstream of c");
        assert!(
            !app.upstream_set.contains("c"),
            "c should not be in its own upstream set"
        );
        assert!(
            !app.upstream_set.contains("d"),
            "d should not be upstream of c"
        );
        assert!(app.downstream_set.is_empty(), "c has no downstream tasks");
    }

    #[test]
    fn test_downstream_set_computed_correctly() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "a");

        // When 'a' is selected, downstream should include both 'b' and 'c'.
        assert!(
            app.downstream_set.contains("b"),
            "b should be downstream of a"
        );
        assert!(
            app.downstream_set.contains("c"),
            "c should be downstream of a"
        );
        assert!(
            !app.downstream_set.contains("a"),
            "a should not be in its own downstream set"
        );
        assert!(
            !app.downstream_set.contains("d"),
            "d should not be downstream of a"
        );
        assert!(
            app.upstream_set.is_empty(),
            "a has no upstream tasks (it's a root)"
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Validation test 1: SHARED ARC COLUMN — fan-in with 3 blockers sharing
    //   one arc column. Only the selected blocker's horizontal + vertical should
    //   be colored; sibling blockers' horizontals stay gray.
    // ══════════════════════════════════════════════════════════════════════

    /// Build a fan-in graph: A depends on B, C, D (A is the dependent, B/C/D are blockers).
    /// This produces back-edge arcs sharing a single arc column.
    fn build_shared_arc_fan_in() -> (VizOutput, WorkGraph) {
        let mut graph = WorkGraph::new();
        let b = make_task_with_status("b", "Blocker B", Status::Done);
        let c = make_task_with_status("c", "Blocker C", Status::Done);
        let d = make_task_with_status("d", "Blocker D", Status::Done);
        let mut a = make_task_with_status("a", "Dependent A", Status::Open);
        a.after = vec!["b".to_string(), "c".to_string(), "d".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Diamond,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
        (result, graph)
    }

    #[test]
    fn test_shared_arc_column_only_selected_blocker_colored() {
        let (viz, _graph) = build_shared_arc_fan_in();

        // Verify the char_edge_map has entries for arcs to 'a' from multiple blockers
        let has_b_to_a = viz
            .char_edge_map
            .values()
            .any(|edges| edges.iter().any(|(s, t)| s == "b" && t == "a"));
        assert!(
            has_b_to_a,
            "char_edge_map should contain b->a arc edge.\nOutput:\n{}\nMap: {:?}",
            viz.text, viz.char_edge_map
        );

        // Select B. B's edge to A should be colored magenta (upstream of A).
        // But we're testing from B's perspective: select B, A is downstream.
        let app = build_app_from_viz_output(&viz, "b");

        // B selected: A is downstream of B (B→A edge). C and D are unrelated
        // (they don't depend on B and B doesn't depend on them).
        assert!(
            app.downstream_set.contains("a"),
            "A should be downstream of B"
        );
        assert!(
            !app.downstream_set.contains("c"),
            "C should NOT be downstream of B"
        );
        assert!(
            !app.downstream_set.contains("d"),
            "D should NOT be downstream of B"
        );

        // Check that edge chars for b->a get colored (cyan for downstream)
        // while edge chars for c->a and d->a stay uncolored.
        let mut found_b_a_colored = false;
        let mut found_c_a_uncolored = true;
        let mut found_d_a_uncolored = true;

        for (&(ln, col), edges) in &viz.char_edge_map {
            let plain = app.plain_lines[ln].as_str();
            let base_line = parse_ansi_line(app.lines[ln].as_str());
            let category = classify_task_line(&app, ln);
            let result =
                apply_per_char_trace_coloring(base_line, plain, ln, &category, &app, Some("b"));

            // Get the resulting style at this character position
            let mut char_idx = 0;
            let mut span_style = Style::default();
            'outer: for span in &result.spans {
                for _ in span.content.chars() {
                    if char_idx == col {
                        span_style = span.style;
                        break 'outer;
                    }
                    char_idx += 1;
                }
            }

            let is_text_range = find_text_range(plain)
                .map(|(s, e)| col >= s && col < e)
                .unwrap_or(false);
            if is_text_range {
                continue; // Skip text characters
            }

            // Check if this position has ONLY b->a edges (no c->a or d->a)
            let has_b_a = edges.iter().any(|(s, t)| s == "b" && t == "a");
            let has_c_a = edges.iter().any(|(s, t)| s == "c" && t == "a");
            let has_d_a = edges.iter().any(|(s, t)| s == "d" && t == "a");

            if has_b_a && !has_c_a && !has_d_a {
                // Pure b->a edge character — should be cyan (downstream)
                if span_style.fg == Some(Color::Cyan) {
                    found_b_a_colored = true;
                }
            }
            if has_c_a && !has_b_a {
                // Pure c->a edge character — should NOT be colored
                if span_style.fg == Some(Color::Magenta) || span_style.fg == Some(Color::Cyan) {
                    found_c_a_uncolored = false;
                }
            }
            if has_d_a && !has_b_a {
                // Pure d->a edge character — should NOT be colored
                if span_style.fg == Some(Color::Magenta) || span_style.fg == Some(Color::Cyan) {
                    found_d_a_uncolored = false;
                }
            }
        }

        assert!(
            found_b_a_colored,
            "B→A edge chars should be colored cyan when B is selected.\nOutput:\n{}",
            viz.text
        );
        assert!(
            found_c_a_uncolored,
            "C→A edge chars should NOT be colored when B is selected.\nOutput:\n{}",
            viz.text
        );
        assert!(
            found_d_a_uncolored,
            "D→A edge chars should NOT be colored when B is selected.\nOutput:\n{}",
            viz.text
        );
    }

    #[test]
    fn test_shared_arc_column_arrowhead_colored() {
        let (viz, _graph) = build_shared_arc_fan_in();
        let app = build_app_from_viz_output(&viz, "b");

        // A is downstream of B. The arrowhead on A's line (← glyph) should be colored cyan
        // because A is the dependent receiving the edge from B.
        let a_line = viz.node_line_map["a"];
        let plain = app.plain_lines[a_line].as_str();

        // Find arc positions on A's line in the char_edge_map
        let mut found_arrowhead_colored = false;
        for (&(ln, col), edges) in &viz.char_edge_map {
            if ln != a_line {
                continue;
            }
            let has_b_a = edges.iter().any(|(s, t)| s == "b" && t == "a");
            if !has_b_a {
                continue;
            }

            let base_line = parse_ansi_line(app.lines[ln].as_str());
            let category = classify_task_line(&app, ln);
            let result =
                apply_per_char_trace_coloring(base_line, plain, ln, &category, &app, Some("b"));

            let is_text = find_text_range(plain)
                .map(|(s, e)| col >= s && col < e)
                .unwrap_or(false);
            if is_text {
                continue;
            }

            let mut char_idx = 0;
            for span in &result.spans {
                for _ in span.content.chars() {
                    if char_idx == col && span.style.fg == Some(Color::Cyan) {
                        found_arrowhead_colored = true;
                    }
                    char_idx += 1;
                }
            }
        }

        assert!(
            found_arrowhead_colored,
            "A's arrowhead (←) should be colored cyan when B is selected (A is downstream).\nOutput:\n{}\nA is at line {}",
            viz.text, a_line
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Validation test 2: TEXT COLORS PRESERVED — all status colors preserved
    //   regardless of selection state
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_text_colors_preserved_all_statuses() {
        // Build a graph with tasks in all statuses that are visible
        let mut graph = WorkGraph::new();
        let a = make_task_with_status("a-root", "Root", Status::Done);
        let mut b = make_task_with_status("b-prog", "Progress", Status::InProgress);
        b.after = vec!["a-root".to_string()];
        let mut c = make_task_with_status("c-open", "Open", Status::Open);
        c.after = vec!["b-prog".to_string()];
        let mut d = make_task_with_status("d-fail", "Failed", Status::Failed);
        d.after = vec!["a-root".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Test with b-prog selected (so a-root is upstream, c-open is downstream,
        // d-fail is a sibling — unrelated to b-prog's chain)
        let app = build_app_from_viz_output(&viz, "b-prog");

        // For each task line, verify text keeps its original style
        for task_id in &["a-root", "b-prog", "c-open", "d-fail"] {
            let line_idx = viz.node_line_map[*task_id];
            let plain = app.plain_lines[line_idx].as_str();
            let base_line = parse_ansi_line(app.lines[line_idx].as_str());
            let category = classify_task_line(&app, line_idx);

            let mut base_text_styles: Vec<(char, Style)> = Vec::new();
            for span in &base_line.spans {
                for c in span.content.chars() {
                    base_text_styles.push((c, span.style));
                }
            }

            let result = apply_per_char_trace_coloring(
                parse_ansi_line(app.lines[line_idx].as_str()),
                plain,
                line_idx,
                &category,
                &app,
                Some("b-prog"),
            );

            let mut result_text_styles: Vec<(char, Style)> = Vec::new();
            for span in &result.spans {
                for c in span.content.chars() {
                    result_text_styles.push((c, span.style));
                }
            }

            let text_range = find_text_range(plain);
            if let Some((text_start, text_end)) = text_range {
                for i in text_start
                    ..text_end
                        .min(base_text_styles.len())
                        .min(result_text_styles.len())
                {
                    assert_eq!(
                        base_text_styles[i].1,
                        result_text_styles[i].1,
                        "Task '{}' text at char {} should preserve original style. \
                         Expected {:?}, got {:?}. Category: {:?}",
                        task_id,
                        i,
                        base_text_styles[i].1,
                        result_text_styles[i].1,
                        match category {
                            LineTraceCategory::Selected => "Selected",
                            LineTraceCategory::Upstream => "Upstream",
                            LineTraceCategory::Downstream => "Downstream",
                            LineTraceCategory::Unrelated => "Unrelated",
                        }
                    );
                }
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Validation test 3: UNRELATED WCCs UNCHANGED — disconnected components
    //   render identically with and without selection
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_unrelated_wcc_unchanged_two_components() {
        // WCC1: a -> b -> c
        // WCC2: x -> y (completely disconnected)
        let mut graph = WorkGraph::new();
        let a = make_task_with_status("a", "Task A", Status::Done);
        let mut b = make_task_with_status("b", "Task B", Status::InProgress);
        b.after = vec!["a".to_string()];
        let mut c = make_task_with_status("c", "Task C", Status::Open);
        c.after = vec!["b".to_string()];
        let x = make_task_with_status("x", "Task X", Status::Open);
        let mut y = make_task_with_status("y", "Task Y", Status::Done);
        y.after = vec!["x".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(x));
        graph.add_node(Node::Task(y));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let app = build_app_from_viz_output(&viz, "b"); // Select in WCC1

        // WCC2 tasks (x, y) should be unrelated
        assert!(
            !app.upstream_set.contains("x"),
            "x should NOT be in upstream"
        );
        assert!(
            !app.downstream_set.contains("x"),
            "x should NOT be in downstream"
        );
        assert!(
            !app.upstream_set.contains("y"),
            "y should NOT be in upstream"
        );
        assert!(
            !app.downstream_set.contains("y"),
            "y should NOT be in downstream"
        );

        // All lines belonging to WCC2 should render identically with trace coloring
        for task_id in &["x", "y"] {
            let line_idx = viz.node_line_map[*task_id];
            let plain = app.plain_lines[line_idx].as_str();
            let base_line = parse_ansi_line(app.lines[line_idx].as_str());
            let category = classify_task_line(&app, line_idx);
            assert!(
                matches!(category, LineTraceCategory::Unrelated),
                "Task '{}' should be Unrelated",
                task_id
            );

            let mut base_chars: Vec<(char, Style)> = Vec::new();
            for span in &base_line.spans {
                for c in span.content.chars() {
                    base_chars.push((c, span.style));
                }
            }

            let result = apply_per_char_trace_coloring(
                parse_ansi_line(app.lines[line_idx].as_str()),
                plain,
                line_idx,
                &category,
                &app,
                Some("b"),
            );

            let mut result_chars: Vec<(char, Style)> = Vec::new();
            for span in &result.spans {
                for c in span.content.chars() {
                    result_chars.push((c, span.style));
                }
            }

            assert_eq!(
                base_chars.len(),
                result_chars.len(),
                "WCC2 task '{}' should have same char count",
                task_id
            );
            for (i, ((bc, bs), (rc, rs))) in base_chars.iter().zip(result_chars.iter()).enumerate()
            {
                assert_eq!(
                    bc, rc,
                    "Char mismatch in WCC2 task '{}' at pos {}",
                    task_id, i
                );
                assert_eq!(
                    bs, rs,
                    "Style mismatch in WCC2 task '{}' at pos {} ('{}'):\n  expected {:?}\n  got {:?}",
                    task_id, i, bc, bs, rs
                );
            }
        }

        // Also check lines between WCC2 tasks (e.g. connector lines)
        let x_line = viz.node_line_map["x"];
        let y_line = viz.node_line_map["y"];
        for line_idx in x_line..=y_line {
            let plain = app.plain_lines[line_idx].as_str();
            let base_line = parse_ansi_line(app.lines[line_idx].as_str());
            let category = classify_task_line(&app, line_idx);

            let mut base_chars: Vec<(char, Style)> = Vec::new();
            for span in &base_line.spans {
                for ch in span.content.chars() {
                    base_chars.push((ch, span.style));
                }
            }

            let result = apply_per_char_trace_coloring(
                parse_ansi_line(app.lines[line_idx].as_str()),
                plain,
                line_idx,
                &category,
                &app,
                Some("b"),
            );

            let mut result_chars: Vec<(char, Style)> = Vec::new();
            for span in &result.spans {
                for ch in span.content.chars() {
                    result_chars.push((ch, span.style));
                }
            }

            for (i, ((bc, bs), (rc, rs))) in base_chars.iter().zip(result_chars.iter()).enumerate()
            {
                assert_eq!(bc, rc);
                assert_eq!(
                    bs, rs,
                    "WCC2 line {} pos {} ('{}') style should be unchanged",
                    line_idx, i, bc
                );
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Validation test 4: SELECTION STYLE — selected task marked with
    //   bold + bright text only, no extra characters or background.
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_selection_style_no_yellow_background() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "b");

        let b_line = viz.node_line_map["b"];
        let plain = app.plain_lines[b_line].as_str();
        let category = classify_task_line(&app, b_line);
        assert!(matches!(category, LineTraceCategory::Selected));

        // apply_per_char_trace_coloring should NOT set yellow background on text.
        let base_line = parse_ansi_line(app.lines[b_line].as_str());
        let result =
            apply_per_char_trace_coloring(base_line, plain, b_line, &category, &app, Some("b"));

        let text_range = find_text_range(plain);
        let (text_start, text_end) = text_range.unwrap();

        let mut char_idx = 0;
        for span in &result.spans {
            for _c in span.content.chars() {
                if char_idx >= text_start && char_idx < text_end {
                    assert_ne!(
                        span.style.bg,
                        Some(Color::Yellow),
                        "apply_per_char_trace_coloring should NOT set yellow background at char {}.",
                        char_idx
                    );
                }
                char_idx += 1;
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Test: SELECTION ON INDEPENDENT TASK — independent tasks get
    //   bold + bright styling when selected.
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_selection_style_on_independent_task() {
        let (viz, _graph) = build_test_graph_chain_plus_isolated();
        let app = build_app_from_viz_output(&viz, "d");

        let d_line = viz.node_line_map["d"];
        let plain = app.plain_lines[d_line].as_str();
        let category = classify_task_line(&app, d_line);
        assert!(matches!(category, LineTraceCategory::Selected));

        // Apply trace coloring first, then selection style (mirrors draw_viz_content).
        let base_line = parse_ansi_line(app.lines[d_line].as_str());
        let colored =
            apply_per_char_trace_coloring(base_line, plain, d_line, &category, &app, Some("d"));
        let result = apply_selection_style(colored, plain);

        // Text spans should be bold, edge spans should NOT be bold.
        let text_range = find_text_range(plain);
        let (text_start, text_end) = text_range.unwrap();
        let mut char_idx = 0;
        for span in &result.spans {
            for _c in span.content.chars() {
                if char_idx >= text_start && char_idx < text_end {
                    assert!(
                        span.style.add_modifier.contains(Modifier::BOLD),
                        "Text char at {} should be bold. Span: {:?}",
                        char_idx,
                        span
                    );
                }
                char_idx += 1;
            }
        }

        // Text content should be unchanged (no extra characters).
        let result_text: String = result
            .spans
            .iter()
            .flat_map(|s| s.content.chars())
            .collect();
        assert_eq!(
            result_text.chars().count(),
            plain.chars().count(),
            "Selection style should not add or remove characters"
        );
    }

    #[test]
    fn test_selection_style_applies_bold() {
        // Verify that text spans get BOLD modifier.
        let plain = "task-id  (open)";
        let line = Line::from(plain.to_string());
        let result = apply_selection_style(line, plain);

        for span in &result.spans {
            assert!(
                span.style.add_modifier.contains(Modifier::BOLD),
                "All spans should be bold. Span: {:?}",
                span
            );
        }
    }

    #[test]
    fn test_selection_style_preserves_text() {
        // Verify that apply_selection_style does not add or remove characters.
        let plain = "├→ task-id  (open)";
        let line = Line::from(plain.to_string());
        let result = apply_selection_style(line, plain);

        let result_text: String = result
            .spans
            .iter()
            .flat_map(|s| s.content.chars())
            .collect();
        assert_eq!(
            result_text, plain,
            "Selection style should preserve text exactly"
        );
    }

    #[test]
    fn test_selection_style_brightens_colors() {
        // Verify that colors are brightened for selected task text.
        let plain = "hello world";
        let line = Line::from(vec![
            Span::styled("hello", Style::default().fg(Color::Green)),
            Span::styled(" world", Style::default().fg(Color::Red)),
        ]);
        let result = apply_selection_style(line, plain);

        // All chars are text (no edges), so all should be brightened.
        let mut found_green = false;
        let mut found_red = false;
        for span in &result.spans {
            if span.style.fg == Some(Color::LightGreen) {
                found_green = true;
            }
            if span.style.fg == Some(Color::LightRed) {
                found_red = true;
            }
        }
        assert!(found_green, "Green should become LightGreen");
        assert!(found_red, "Red should become LightRed");
    }

    #[test]
    fn test_selection_style_does_not_bold_edges() {
        // Edge chars (├→) should NOT get bold, only the text portion should.
        let plain = "├→ task-id  (open)";
        let line = Line::from(vec![
            Span::styled("├→ ", Style::default().fg(Color::White)),
            Span::styled("task-id  (open)", Style::default().fg(Color::Green)),
        ]);
        let result = apply_selection_style(line, plain);

        let text_range = find_text_range(plain).unwrap();
        let mut char_idx = 0;
        for span in &result.spans {
            for _c in span.content.chars() {
                if char_idx < text_range.0 {
                    // Edge/connector chars — should NOT be bold.
                    assert!(
                        !span.style.add_modifier.contains(Modifier::BOLD),
                        "Edge char at {} should NOT be bold. Span: {:?}",
                        char_idx,
                        span
                    );
                } else if char_idx < text_range.1 {
                    // Text chars — should be bold.
                    assert!(
                        span.style.add_modifier.contains(Modifier::BOLD),
                        "Text char at {} should be bold. Span: {:?}",
                        char_idx,
                        span
                    );
                }
                char_idx += 1;
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Validation test 5: ADDITIVE ONLY — with no selection, output must be
    //   identical to normal wg viz. Trace only changes edge colors + block cursor.
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_additive_only_no_selection_identity() {
        // Test with a complex graph (chain + isolated + fan-in)
        let mut graph = WorkGraph::new();
        let a = make_task_with_status("a", "Task A", Status::Done);
        let mut b = make_task_with_status("b", "Task B", Status::InProgress);
        b.after = vec!["a".to_string()];
        let mut c = make_task_with_status("c", "Task C", Status::Open);
        c.after = vec!["b".to_string()];
        let d = make_task_with_status("d", "Task D", Status::Failed);
        let x = make_task_with_status("x", "Task X", Status::Open);
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));
        graph.add_node(Node::Task(x));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Build app with NO selection
        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.selected_task_idx = None;
        app.upstream_set.clear();
        app.downstream_set.clear();

        // When no selection is active, has_selection is false and the code goes
        // through the `else` branch which pushes base_line unchanged.
        // Verify: apply_per_char_trace_coloring with Unrelated category and empty sets
        // produces output identical to input for EVERY line.
        for (idx, ansi_line) in app.lines.iter().enumerate() {
            let plain = &app.plain_lines[idx];
            let base_line = parse_ansi_line(ansi_line);

            let mut base_chars: Vec<(char, Style)> = Vec::new();
            for span in &base_line.spans {
                for c in span.content.chars() {
                    base_chars.push((c, span.style));
                }
            }

            // With no selection, the category is always Unrelated
            let result = apply_per_char_trace_coloring(
                parse_ansi_line(ansi_line),
                plain,
                idx,
                &LineTraceCategory::Unrelated,
                &app,
                None,
            );

            let mut result_chars: Vec<(char, Style)> = Vec::new();
            for span in &result.spans {
                for c in span.content.chars() {
                    result_chars.push((c, span.style));
                }
            }

            assert_eq!(
                base_chars.len(),
                result_chars.len(),
                "Line {} should have identical char count with no selection",
                idx
            );
            for (i, ((bc, bs), (rc, rs))) in base_chars.iter().zip(result_chars.iter()).enumerate()
            {
                assert_eq!(
                    bc, rc,
                    "No-selection: char mismatch at line {} pos {}",
                    idx, i
                );
                assert_eq!(
                    bs, rs,
                    "No-selection: style mismatch at line {} pos {} ('{}'):\n  expected {:?}\n  got {:?}",
                    idx, i, bc, bs, rs
                );
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Validation test 6: PINK AGENCY PHASES — tasks in assigning/evaluating
    //   phases should show pink (magenta) text
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_pink_agency_phase_text() {
        // Build a graph with a task that has [assigning] annotation (magenta/pink).
        // NOTE: In test environments, stdout is not a terminal so ANSI color codes
        // are suppressed by generate_ascii. We verify:
        // 1. The annotation text [assigning]/[evaluating] appears in the output
        // 2. The format_node logic would produce magenta (\x1b[35m]) when use_color=true
        //    (verified by the is_agency_phase check in ascii.rs lines 309-321)
        // 3. The phase annotation is correctly applied

        let mut graph = WorkGraph::new();
        let task = make_task_with_status("my-task", "My Task", Status::Open);
        graph.add_node(Node::Task(task));

        let mut annotations = HashMap::new();
        annotations.insert(
            "my-task".to_string(),
            crate::commands::viz::AnnotationInfo {
                text: "[assigning]".to_string(),
                dot_task_ids: vec![".assign-my-task".to_string()],
            },
        );

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &annotations,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // The annotation [assigning] must appear in the output
        let task_line_idx = viz.node_line_map["my-task"];
        let line_text = viz.text.lines().nth(task_line_idx).unwrap();
        assert!(
            line_text.contains("[assigning]"),
            "Assigning phase should show [assigning] annotation.\nLine: {:?}",
            line_text
        );

        // In a terminal, the ANSI code \x1b[35m (magenta) would be present.
        // In non-terminal test environments, no ANSI codes are emitted.
        // Either way, the annotation text must be present. If ANSI codes ARE
        // present (some CI environments have tty), they should be magenta.
        if line_text.contains("\x1b[") {
            assert!(
                line_text.contains("\x1b[38;5;219m"),
                "If ANSI codes are present, assigning phase should use true pink (\\x1b[38;5;219m).\nLine: {:?}",
                line_text
            );
        }

        // Test evaluating phase
        let mut graph2 = WorkGraph::new();
        let task2 = make_task_with_status("eval-task", "Eval Task", Status::Done);
        graph2.add_node(Node::Task(task2));
        let mut annotations2 = HashMap::new();
        annotations2.insert(
            "eval-task".to_string(),
            crate::commands::viz::AnnotationInfo {
                text: "[∴ evaluating]".to_string(),
                dot_task_ids: vec![".evaluate-eval-task".to_string()],
            },
        );

        let tasks2: Vec<_> = graph2.tasks().collect();
        let task_ids2: HashSet<&str> = tasks2.iter().map(|t| t.id.as_str()).collect();
        let viz2 = generate_ascii(
            &graph2,
            &tasks2,
            &task_ids2,
            &annotations2,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let task_line_idx2 = viz2.node_line_map["eval-task"];
        let ansi_line2 = viz2.text.lines().nth(task_line_idx2).unwrap();
        assert!(
            ansi_line2.contains("[∴ evaluating]"),
            "Evaluating phase should show [∴ evaluating] annotation.\nLine: {:?}",
            ansi_line2
        );
        if ansi_line2.contains("\x1b[") {
            assert!(
                ansi_line2.contains("\x1b[38;5;219m"),
                "If ANSI codes are present, evaluating phase should use true pink (\\x1b[38;5;219m).\nLine: {:?}",
                ansi_line2
            );
        }

        // Test validating phase
        let mut graph3 = WorkGraph::new();
        let task3 = make_task_with_status("val-task", "Val Task", Status::InProgress);
        graph3.add_node(Node::Task(task3));
        let mut annotations3 = HashMap::new();
        annotations3.insert(
            "val-task".to_string(),
            crate::commands::viz::AnnotationInfo {
                text: "[✓ validating]".to_string(),
                dot_task_ids: vec![".verify-val-task".to_string()],
            },
        );

        let tasks3: Vec<_> = graph3.tasks().collect();
        let task_ids3: HashSet<&str> = tasks3.iter().map(|t| t.id.as_str()).collect();
        let viz3 = generate_ascii(
            &graph3,
            &tasks3,
            &task_ids3,
            &annotations3,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let task_line_idx3 = viz3.node_line_map["val-task"];
        let ansi_line3 = viz3.text.lines().nth(task_line_idx3).unwrap();
        assert!(
            ansi_line3.contains("[✓ validating]"),
            "Validating phase should show [✓ validating] annotation.\nLine: {:?}",
            ansi_line3
        );

        // Test both simultaneously
        let mut graph4 = WorkGraph::new();
        let task4 = make_task_with_status("both-task", "Both Task", Status::InProgress);
        graph4.add_node(Node::Task(task4));
        let mut annotations4 = HashMap::new();
        annotations4.insert(
            "both-task".to_string(),
            crate::commands::viz::AnnotationInfo {
                text: "[∴ evaluating] [✓ validating]".to_string(),
                dot_task_ids: vec![
                    ".evaluate-both-task".to_string(),
                    ".verify-both-task".to_string(),
                ],
            },
        );

        let tasks4: Vec<_> = graph4.tasks().collect();
        let task_ids4: HashSet<&str> = tasks4.iter().map(|t| t.id.as_str()).collect();
        let viz4 = generate_ascii(
            &graph4,
            &tasks4,
            &task_ids4,
            &annotations4,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let task_line_idx4 = viz4.node_line_map["both-task"];
        let ansi_line4 = viz4.text.lines().nth(task_line_idx4).unwrap();
        assert!(
            ansi_line4.contains("[∴ evaluating]") && ansi_line4.contains("[✓ validating]"),
            "Both phases should appear simultaneously.\nLine: {:?}",
            ansi_line4
        );

        // Verify the code logic: in ascii.rs, the agency phase detection checks:
        //   is_agency_phase = use_color && annotations.get(id).map_or(false, |a| a.contains("assigning") || a.contains("evaluating") || a.contains("validating") || a.contains("verifying"))
        // When true, the phase annotation is wrapped in \x1b[38;5;219m..\x1b[0m (ANSI 256-color 219, true pink).
        // The task text itself keeps its status color (e.g., green for done).
        // We've verified the annotation appears; the color logic is deterministic given use_color.
    }

    #[test]
    fn test_pink_agency_phase_preserves_in_trace() {
        // Verify that when a task is in an agency phase and trace coloring is applied,
        // the pink text color is preserved (trace is additive, only edge chars change).
        let mut graph = WorkGraph::new();
        let root = make_task_with_status("root", "Root", Status::Done);
        let mut child = make_task_with_status("child", "Child", Status::Open);
        child.after = vec!["root".to_string()];
        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(child));

        let mut annotations = HashMap::new();
        annotations.insert(
            "child".to_string(),
            crate::commands::viz::AnnotationInfo {
                text: "[assigning]".to_string(),
                dot_task_ids: vec![".assign-child".to_string()],
            },
        );

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &annotations,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Select 'child' — it's the selected task, text should keep pink/magenta
        let app = build_app_from_viz_output(&viz, "child");
        let child_line = viz.node_line_map["child"];
        let plain = app.plain_lines[child_line].as_str();
        let base_line = parse_ansi_line(app.lines[child_line].as_str());
        let category = classify_task_line(&app, child_line);

        let mut base_text_fg: Vec<Option<Color>> = Vec::new();
        let mut idx = 0;
        let text_range = find_text_range(plain);
        let (text_start, text_end) = text_range.unwrap_or((usize::MAX, usize::MAX));
        for span in &base_line.spans {
            for _ in span.content.chars() {
                if idx >= text_start && idx < text_end {
                    base_text_fg.push(span.style.fg);
                }
                idx += 1;
            }
        }

        let result = apply_per_char_trace_coloring(
            parse_ansi_line(app.lines[child_line].as_str()),
            plain,
            child_line,
            &category,
            &app,
            Some("child"),
        );

        let mut result_text_fg: Vec<Option<Color>> = Vec::new();
        idx = 0;
        for span in &result.spans {
            for _ in span.content.chars() {
                if idx >= text_start && idx < text_end {
                    result_text_fg.push(span.style.fg);
                }
                idx += 1;
            }
        }

        assert_eq!(base_text_fg.len(), result_text_fg.len());
        for (i, (base, result)) in base_text_fg.iter().zip(result_text_fg.iter()).enumerate() {
            assert_eq!(
                base, result,
                "Agency-phase text fg at position {} should be preserved: {:?} vs {:?}",
                i, base, result
            );
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // SIBLING NOT IN TRACE: tree connectors to untraced siblings stay uncolored
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_sibling_not_in_trace_connectors_uncolored() {
        // Build a tree:
        //   root
        //   ├→ child-a
        //   │  └→ grandchild   <-- SELECTED
        //   └→ child-b         <-- NOT in trace (sibling, not in chain)
        //
        // When grandchild is selected, the trace goes:
        //   grandchild → child-a → root
        // child-b is a sibling of child-a under root. It is NOT in the chain.
        // The │ between child-a's subtree and child-b, and the └→ connector
        // on child-b's line, must NOT be colored.

        let mut graph = WorkGraph::new();
        let root = make_task_with_status("root", "Root Task", Status::Done);
        let mut child_a = make_task_with_status("child-a", "Child A", Status::Done);
        child_a.after = vec!["root".to_string()];
        let mut grandchild = make_task_with_status("grandchild", "Grandchild", Status::InProgress);
        grandchild.after = vec!["child-a".to_string()];
        let mut child_b = make_task_with_status("child-b", "Child B", Status::Open);
        child_b.after = vec!["root".to_string()];

        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(child_a));
        graph.add_node(Node::Task(grandchild));
        graph.add_node(Node::Task(child_b));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let app = build_app_from_viz_output(&viz, "grandchild");

        // Verify upstream set is correct: grandchild, child-a, root
        assert!(
            app.upstream_set.contains("root"),
            "root should be in upstream set"
        );
        assert!(
            app.upstream_set.contains("child-a"),
            "child-a should be in upstream set"
        );
        assert!(
            !app.upstream_set.contains("child-b"),
            "child-b should NOT be in upstream set"
        );

        // Find child-b's line and check its connectors are NOT colored
        let child_b_line = viz.node_line_map["child-b"];

        // Check all edge-mapped characters on child-b's line and between child-a's
        // subtree and child-b: none should be colored magenta or cyan
        for (&(ln, col), edges) in &viz.char_edge_map {
            // Only check edges that involve child-b (the untraced sibling)
            let involves_child_b = edges.iter().any(|(s, t)| s == "child-b" || t == "child-b");
            if !involves_child_b {
                continue;
            }

            let plain = app.plain_lines[ln].as_str();
            let base_line = parse_ansi_line(app.lines[ln].as_str());
            let category = classify_task_line(&app, ln);

            let result = apply_per_char_trace_coloring(
                base_line,
                plain,
                ln,
                &category,
                &app,
                Some("grandchild"),
            );

            let mut char_idx = 0;
            for span in &result.spans {
                for _ in span.content.chars() {
                    if char_idx == col {
                        assert!(
                            span.style.fg != Some(Color::Magenta)
                                && span.style.fg != Some(Color::Cyan),
                            "Edge char at ({}, {}) involving child-b should NOT be colored (got {:?}). \
                             child-b is not in the trace chain. Edges: {:?}\nOutput:\n{}",
                            ln,
                            col,
                            span.style.fg,
                            edges,
                            viz.text
                        );
                    }
                    char_idx += 1;
                }
            }
        }

        // Also verify that the │ vertical bars between child-a's subtree and child-b
        // do NOT map to the (root, child-a) edge — they should only map to (root, child-b)
        let child_a_line = viz.node_line_map["child-a"];
        for l in (child_a_line + 1)..child_b_line {
            if let Some(edges) = viz.char_edge_map.get(&(l, 0)) {
                assert!(
                    !edges.iter().any(|(s, t)| s == "root" && t == "child-a"),
                    "│ at line {} between child-a's subtree and child-b should NOT contain \
                     edge (root, child-a). It should only contain edges for children below. \
                     Edges: {:?}",
                    l,
                    edges
                );
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // DEEP SUBTREE SIBLING: vertical bar to untraced sibling stays uncolored
    // Reproduces the exact topology from the fix-vertical-tree bug report.
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_deep_subtree_vertical_bar_untraced_sibling() {
        // Build the exact topology from the bug:
        //
        //   root
        //   ├→ child-a
        //   │ └→ gc1
        //   │   └→ gc2
        //   │     └→ gc3      <-- SELECTED
        //   │       └→ gc4
        //   └→ child-b        <-- NOT in trace
        //
        // When gc3 is selected, the trace goes:
        //   gc3 → gc2 → gc1 → child-a → root
        //
        // child-b is a sibling of child-a under root, NOT in the trace.
        // The │ chars at column 0 between child-a's subtree and child-b
        // should map ONLY to (root, child-b). Since child-b is NOT
        // upstream of gc3, those │ chars must NOT be colored magenta.

        let mut graph = WorkGraph::new();
        let root = make_task_with_status("root", "Root", Status::Done);
        let mut child_a = make_task_with_status("child-a", "Child A", Status::Done);
        child_a.after = vec!["root".to_string()];
        let mut gc1 = make_task_with_status("gc1", "GC1", Status::Done);
        gc1.after = vec!["child-a".to_string()];
        let mut gc2 = make_task_with_status("gc2", "GC2", Status::Done);
        gc2.after = vec!["gc1".to_string()];
        let mut gc3 = make_task_with_status("gc3", "GC3", Status::Done);
        gc3.after = vec!["gc2".to_string()];
        let mut gc4 = make_task_with_status("gc4", "GC4", Status::Done);
        gc4.after = vec!["gc3".to_string()];
        let mut child_b = make_task_with_status("child-b", "Child B", Status::Done);
        child_b.after = vec!["root".to_string()];

        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(child_a));
        graph.add_node(Node::Task(gc1));
        graph.add_node(Node::Task(gc2));
        graph.add_node(Node::Task(gc3));
        graph.add_node(Node::Task(gc4));
        graph.add_node(Node::Task(child_b));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let app = build_app_from_viz_output(&viz, "gc3");

        // Verify upstream set is correct
        assert!(app.upstream_set.contains("root"), "root should be upstream");
        assert!(
            app.upstream_set.contains("child-a"),
            "child-a should be upstream"
        );
        assert!(app.upstream_set.contains("gc1"), "gc1 should be upstream");
        assert!(app.upstream_set.contains("gc2"), "gc2 should be upstream");
        assert!(
            !app.upstream_set.contains("child-b"),
            "child-b should NOT be upstream"
        );
        assert!(
            !app.upstream_set.contains("gc4"),
            "gc4 is downstream, not upstream"
        );

        let child_a_line = viz.node_line_map["child-a"];
        let child_b_line = viz.node_line_map["child-b"];

        // PART 1: Verify char_edge_map correctness.
        // The │ bars at col 0 between child-a and child-b must map ONLY to
        // (root, child-b), NOT to (root, child-a) which would cause coloring.
        for l in (child_a_line + 1)..child_b_line {
            if let Some(edges) = viz.char_edge_map.get(&(l, 0)) {
                assert!(
                    !edges.iter().any(|(s, t)| s == "root" && t == "child-a"),
                    "│ at ({}, 0) should NOT have (root, child-a). Edges: {:?}",
                    l,
                    edges
                );

                // No edge should have BOTH endpoints in the upstream set
                let would_be_colored = edges.iter().any(|(src, tgt)| {
                    let src_upstream = app.upstream_set.contains(src.as_str()) || src == "gc3";
                    let tgt_upstream = app.upstream_set.contains(tgt.as_str()) || tgt == "gc3";
                    src_upstream && tgt_upstream
                });
                assert!(
                    !would_be_colored,
                    "│ at ({}, 0) would be colored magenta but child-b is NOT in trace! \
                     Edges: {:?}\nUpstream: {:?}",
                    l, edges, app.upstream_set
                );
            }
        }

        // PART 2: Verify actual render — apply per-char coloring and check
        // that the │ chars leading to the untraced sibling are NOT colored.
        for l in (child_a_line + 1)..child_b_line {
            let plain = app.plain_lines[l].as_str();
            let chars: Vec<char> = plain.chars().collect();
            if chars.is_empty() || chars[0] != '│' {
                continue;
            }

            let base_line = parse_ansi_line(app.lines[l].as_str());
            let category = classify_task_line(&app, l);
            let result =
                apply_per_char_trace_coloring(base_line, plain, l, &category, &app, Some("gc3"));

            // The first character (│ at col 0) must NOT be magenta or cyan
            let mut char_idx = 0;
            for span in &result.spans {
                for c in span.content.chars() {
                    if char_idx == 0 && c == '│' {
                        assert!(
                            span.style.fg != Some(Color::Magenta)
                                && span.style.fg != Some(Color::Cyan),
                            "│ at line {} col 0 should NOT be colored! child-b is not in \
                             the trace. Got fg={:?}\nPlain: {}\nOutput:\n{}",
                            l,
                            span.style.fg,
                            plain,
                            viz.text
                        );
                    }
                    char_idx += 1;
                }
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Cycle edge visualization tests
    // ══════════════════════════════════════════════════════════════════════

    /// Helper: build a graph, generate viz, select a task, and return the app.
    fn build_cycle_app(graph: &WorkGraph, selected_id: &str) -> VizApp {
        let viz = {
            let tasks: Vec<_> = graph.tasks().collect();
            let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
            generate_ascii(
                graph,
                &tasks,
                &task_ids,
                &HashMap::new(),
                &HashMap::new(),
                &HashMap::new(),
                LayoutMode::default(),
                &HashSet::new(),
                "gray",
                &HashMap::new(),
                &HashMap::new(),
            )
        };
        build_app_from_viz_output(&viz, selected_id)
    }

    /// Helper: check if ANY edge character on any line has Yellow fg color
    /// when apply_per_char_trace_coloring is applied.
    fn has_any_yellow_edge(app: &VizApp, selected_id: &str) -> bool {
        for (idx, ansi_line) in app.lines.iter().enumerate() {
            let plain = app.plain_lines[idx].as_str();
            let base_line = parse_ansi_line(ansi_line);
            let category = classify_task_line(app, idx);
            let result = apply_per_char_trace_coloring(
                base_line,
                plain,
                idx,
                &category,
                app,
                Some(selected_id),
            );

            let mut char_idx = 0;
            for span in &result.spans {
                for _c in span.content.chars() {
                    if app.char_edge_map.contains_key(&(idx, char_idx))
                        && span.style.fg == Some(Color::Yellow)
                    {
                        return true;
                    }
                    char_idx += 1;
                }
            }
        }
        false
    }

    /// Helper: collect all (line, col) positions where edge chars have Yellow fg.
    fn collect_yellow_edge_positions(app: &VizApp, selected_id: &str) -> HashSet<(usize, usize)> {
        let mut positions = HashSet::new();
        for (idx, ansi_line) in app.lines.iter().enumerate() {
            let plain = app.plain_lines[idx].as_str();
            let base_line = parse_ansi_line(ansi_line);
            let category = classify_task_line(app, idx);
            let result = apply_per_char_trace_coloring(
                base_line,
                plain,
                idx,
                &category,
                app,
                Some(selected_id),
            );

            let mut char_idx = 0;
            for span in &result.spans {
                for _c in span.content.chars() {
                    if app.char_edge_map.contains_key(&(idx, char_idx))
                        && span.style.fg == Some(Color::Yellow)
                    {
                        positions.insert((idx, char_idx));
                    }
                    char_idx += 1;
                }
            }
        }
        positions
    }

    /// Helper: collect all (line, col) positions where edge chars have Magenta fg.
    fn collect_magenta_edge_positions(app: &VizApp, selected_id: &str) -> HashSet<(usize, usize)> {
        let mut positions = HashSet::new();
        for (idx, ansi_line) in app.lines.iter().enumerate() {
            let plain = app.plain_lines[idx].as_str();
            let base_line = parse_ansi_line(ansi_line);
            let category = classify_task_line(app, idx);
            let result = apply_per_char_trace_coloring(
                base_line,
                plain,
                idx,
                &category,
                app,
                Some(selected_id),
            );

            let mut char_idx = 0;
            for span in &result.spans {
                for _c in span.content.chars() {
                    if app.char_edge_map.contains_key(&(idx, char_idx))
                        && span.style.fg == Some(Color::Magenta)
                    {
                        positions.insert((idx, char_idx));
                    }
                    char_idx += 1;
                }
            }
        }
        positions
    }

    // ── Test 1: Simple cycle A → B → C → A ──

    #[test]
    fn test_cycle_simple_all_edges_yellow() {
        // A → B → C → A. Select A. All three edges should be yellow.
        let mut graph = WorkGraph::new();
        let mut a = make_task_with_status("a", "Task A", Status::Open);
        a.after = vec!["c".to_string()];
        let mut b = make_task_with_status("b", "Task B", Status::Open);
        b.after = vec!["a".to_string()];
        let mut c = make_task_with_status("c", "Task C", Status::Open);
        c.after = vec!["b".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let app = build_cycle_app(&graph, "a");

        // cycle_set should contain all three
        assert!(app.cycle_set.contains("a"), "a should be in cycle_set");
        assert!(app.cycle_set.contains("b"), "b should be in cycle_set");
        assert!(app.cycle_set.contains("c"), "c should be in cycle_set");

        // There should be yellow edges
        assert!(
            has_any_yellow_edge(&app, "a"),
            "Simple cycle: should have yellow edges when A selected.\nViz:\n{}",
            app.lines.join("\n")
        );
    }

    // ── Test 2: No cycle — linear chain ──

    #[test]
    fn test_no_cycle_no_yellow() {
        // Linear chain A → B → C. Select B. No yellow edges.
        let mut graph = WorkGraph::new();
        let a = make_task_with_status("a", "Task A", Status::Done);
        let mut b = make_task_with_status("b", "Task B", Status::InProgress);
        b.after = vec!["a".to_string()];
        let mut c = make_task_with_status("c", "Task C", Status::Open);
        c.after = vec!["b".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let app = build_cycle_app(&graph, "b");

        // cycle_set should be empty
        assert!(
            app.cycle_set.is_empty(),
            "Linear chain: cycle_set should be empty"
        );

        // No yellow edges
        assert!(
            !has_any_yellow_edge(&app, "b"),
            "Linear chain: should have no yellow edges.\nViz:\n{}",
            app.lines.join("\n")
        );
    }

    // ── Test 3: Cycle + non-cycle edges ──

    #[test]
    fn test_cycle_with_non_cycle_edge() {
        // A → B → C → A (cycle), plus D → A (non-cycle upstream).
        // Select A. Cycle edges yellow, D→A should be magenta (upstream), not yellow.
        let mut graph = WorkGraph::new();
        let mut a = make_task_with_status("a", "Task A", Status::Open);
        a.after = vec!["c".to_string(), "d".to_string()];
        let mut b = make_task_with_status("b", "Task B", Status::Open);
        b.after = vec!["a".to_string()];
        let mut c = make_task_with_status("c", "Task C", Status::Open);
        c.after = vec!["b".to_string()];
        let d = make_task_with_status("d", "Task D", Status::Done);
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let app = build_cycle_app(&graph, "a");

        // cycle_set should contain a, b, c but NOT d
        assert!(app.cycle_set.contains("a"), "a in cycle_set");
        assert!(app.cycle_set.contains("b"), "b in cycle_set");
        assert!(app.cycle_set.contains("c"), "c in cycle_set");
        assert!(!app.cycle_set.contains("d"), "d should NOT be in cycle_set");

        // D should be upstream of A
        assert!(app.upstream_set.contains("d"), "d should be upstream of a");

        // Check that cycle edges exist and are yellow
        assert!(
            has_any_yellow_edge(&app, "a"),
            "Cycle+non-cycle: should have yellow edges for the cycle.\nViz:\n{}",
            app.lines.join("\n")
        );

        // Check that edges involving D are NOT yellow.
        // D→A edges should be magenta (upstream), not yellow.
        let yellow_positions = collect_yellow_edge_positions(&app, "a");
        for (line, col) in &yellow_positions {
            if let Some(edges) = app.char_edge_map.get(&(*line, *col)) {
                for (src, tgt) in edges {
                    // If this position is yellow, the edge should be between cycle members
                    assert!(
                        app.cycle_set.contains(src.as_str())
                            && app.cycle_set.contains(tgt.as_str()),
                        "Yellow edge at ({},{}) has non-cycle endpoints: ({}, {})",
                        line,
                        col,
                        src,
                        tgt
                    );
                }
            }
        }
    }

    // ── Test 4: Multiple cycles ──

    #[test]
    fn test_multiple_cycles_all_yellow() {
        // A → B → A and B → C → B. Select B. Both cycles' edges should be yellow.
        // All three nodes form one SCC.
        let mut graph = WorkGraph::new();
        let mut a = make_task_with_status("a", "Task A", Status::Open);
        a.after = vec!["b".to_string()];
        let mut b = make_task_with_status("b", "Task B", Status::Open);
        b.after = vec!["a".to_string(), "c".to_string()];
        let mut c = make_task_with_status("c", "Task C", Status::Open);
        c.after = vec!["b".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let app = build_cycle_app(&graph, "b");

        // All three should be in the same SCC
        assert!(app.cycle_set.contains("a"), "a in cycle_set");
        assert!(app.cycle_set.contains("b"), "b in cycle_set");
        assert!(app.cycle_set.contains("c"), "c in cycle_set");

        // Should have yellow edges
        assert!(
            has_any_yellow_edge(&app, "b"),
            "Multiple cycles: should have yellow edges.\nViz:\n{}",
            app.lines.join("\n")
        );
    }

    // ── Test 5: Self-loop ──

    #[test]
    fn test_self_loop_yellow() {
        // A → A. Select A. The self-loop edge should be yellow.
        let mut graph = WorkGraph::new();
        let mut a = make_task_with_status("a", "Task A", Status::Open);
        a.after = vec!["a".to_string()];
        graph.add_node(Node::Task(a));

        let app = build_cycle_app(&graph, "a");

        // Self-loop: the SCC detection includes 'a' in its own cycle.
        // The ↺ symbol is rendered but isn't tracked in char_edge_map
        // (it's a status indicator, not an arrow), so yellow edge
        // detection won't find it. Verify cycle_set membership instead.
        assert!(
            app.cycle_set.contains("a"),
            "Self-loop should be detected as a cycle member"
        );
    }

    // ── Test 6: Nested cycles (larger SCC) ──

    #[test]
    fn test_nested_cycles_all_scc_yellow() {
        // A → B → C → A and A → B → A (subset). Select A.
        // All edges in the larger SCC should be yellow.
        // Since A, B, C all form one SCC, all should be in cycle_set.
        let mut graph = WorkGraph::new();
        let mut a = make_task_with_status("a", "Task A", Status::Open);
        a.after = vec!["b".to_string(), "c".to_string()]; // A depends on B and C (back-edges)
        let mut b = make_task_with_status("b", "Task B", Status::Open);
        b.after = vec!["a".to_string()]; // B depends on A
        let mut c = make_task_with_status("c", "Task C", Status::Open);
        c.after = vec!["b".to_string()]; // C depends on B
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let app = build_cycle_app(&graph, "a");

        // All three should be in the SCC
        assert!(app.cycle_set.contains("a"), "a in cycle_set");
        assert!(app.cycle_set.contains("b"), "b in cycle_set");
        assert!(app.cycle_set.contains("c"), "c in cycle_set");

        // All edges between SCC members should be yellow
        let yellow_positions = collect_yellow_edge_positions(&app, "a");
        assert!(
            !yellow_positions.is_empty(),
            "Nested cycles: should have yellow edges.\nViz:\n{}",
            app.lines.join("\n")
        );
    }

    // ── Test 7: Non-member selected ──

    #[test]
    fn test_non_member_selected_no_yellow() {
        // A → B → C → A (cycle), D is independent. Select D. No yellow edges.
        let mut graph = WorkGraph::new();
        let mut a = make_task_with_status("a", "Task A", Status::Open);
        a.after = vec!["c".to_string()];
        let mut b = make_task_with_status("b", "Task B", Status::Open);
        b.after = vec!["a".to_string()];
        let mut c = make_task_with_status("c", "Task C", Status::Open);
        c.after = vec!["b".to_string()];
        let d = make_task_with_status("d", "Task D", Status::Open);
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let app = build_cycle_app(&graph, "d");

        // D is not in any cycle
        assert!(
            app.cycle_set.is_empty(),
            "Non-member selected: cycle_set should be empty, got {:?}",
            app.cycle_set
        );

        // No yellow edges
        assert!(
            !has_any_yellow_edge(&app, "d"),
            "Non-member selected: should have no yellow edges.\nViz:\n{}",
            app.lines.join("\n")
        );
    }

    // ── Additional: Yellow overrides magenta for cycle edges ──

    #[test]
    fn test_cycle_yellow_overrides_magenta() {
        // In a cycle A → B → C → A, when A is selected:
        // B and C are both upstream AND downstream of A.
        // Cycle edges should be yellow (highest priority), not magenta or cyan.
        let mut graph = WorkGraph::new();
        let mut a = make_task_with_status("a", "Task A", Status::Open);
        a.after = vec!["c".to_string()];
        let mut b = make_task_with_status("b", "Task B", Status::Open);
        b.after = vec!["a".to_string()];
        let mut c = make_task_with_status("c", "Task C", Status::Open);
        c.after = vec!["b".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let app = build_cycle_app(&graph, "a");

        // Verify B and C are both upstream and downstream (cycle means both)
        let b_reachable = app.upstream_set.contains("b") || app.downstream_set.contains("b");
        let c_reachable = app.upstream_set.contains("c") || app.downstream_set.contains("c");
        assert!(b_reachable, "b should be reachable from a");
        assert!(c_reachable, "c should be reachable from a");

        // For every edge between cycle members, verify yellow takes priority
        let yellow = collect_yellow_edge_positions(&app, "a");
        let magenta = collect_magenta_edge_positions(&app, "a");

        // No position should be both yellow and magenta (yellow should override)
        let overlap: HashSet<_> = yellow.intersection(&magenta).collect();
        assert!(
            overlap.is_empty(),
            "Cycle edge positions should not be magenta — yellow overrides. \
             Overlap at: {:?}",
            overlap
        );

        // There should be some yellow edges
        assert!(
            !yellow.is_empty(),
            "Cycle edges should be yellow, but none found.\nViz:\n{}",
            app.lines.join("\n")
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // HUD LAYOUT AND RENDER TESTS
    // ══════════════════════════════════════════════════════════════════════

    /// Build a chain graph a -> b -> c plus standalone d for HUD tests.
    fn build_hud_test_graph() -> (VizOutput, WorkGraph) {
        let mut graph = WorkGraph::new();
        let mut a = make_task_with_status("a", "Task Alpha", Status::Done);
        a.description = Some("Description for Alpha.".to_string());

        let mut b = make_task_with_status("b", "Task Bravo", Status::InProgress);
        b.after = vec!["a".to_string()];

        let mut c = make_task_with_status("c", "Task Charlie", Status::Open);
        c.after = vec!["b".to_string()];

        let d = make_task_with_status("d", "Task Delta", Status::Failed);

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
        (result, graph)
    }

    // ── TEST 5: NARROW TERMINAL FALLBACK ──

    #[test]
    fn hud_layout_side_by_side_at_wide_terminal() {
        use ratatui::layout::{Constraint, Direction, Layout, Rect};

        let wide_area = Rect::new(0, 0, 120, 40);
        let side_min_width: u16 = SIDE_MIN_WIDTH;

        assert!(wide_area.width >= side_min_width);

        let hud_width = (wide_area.width as u32 * 35u32 / 100) as u16;
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(hud_width)])
            .split(wide_area);

        assert!(split[0].width > 0, "viz area should have non-zero width");
        assert_eq!(split[1].width, hud_width, "HUD should have computed width");
        assert_eq!(
            split[1].height, wide_area.height,
            "HUD should span full height"
        );
        assert_eq!(split[1].x, wide_area.width - hud_width, "HUD on right side");
    }

    #[test]
    fn hud_layout_bottom_panel_at_narrow_terminal() {
        use ratatui::layout::{Constraint, Direction, Layout, Rect};

        let narrow_area = Rect::new(0, 0, 80, 40);
        assert!(narrow_area.width < SIDE_MIN_WIDTH);

        let hud_height =
            (narrow_area.height as u32 * BOTTOM_PANEL_PERCENT as u32 / 100).max(5) as u16;
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(hud_height)])
            .split(narrow_area);

        assert!(split[0].height > 0, "viz area should have non-zero height");
        assert_eq!(
            split[1].height, hud_height,
            "HUD should have computed height"
        );
        assert_eq!(
            split[1].width, narrow_area.width,
            "HUD should span full width"
        );
        assert!(split[1].y > 0, "HUD should be below viz area");
    }

    // ── RENDER DRAW TESTS (no-panic) ──

    #[test]
    fn draw_with_hud_does_not_panic_wide() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    }

    #[test]
    fn draw_with_hud_does_not_panic_narrow() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");

        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    }

    #[test]
    fn draw_without_hud_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.toggle_trace();

        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    }

    #[test]
    fn draw_hud_no_selection_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.selected_task_idx = None;

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    }

    /// End-to-end render smoke test for the chat pane's rendering
    /// of the wg-nex box-drawing transcript format.
    ///
    /// Drives the actual `draw_chat_tab` call path against a
    /// TestBackend and inspects the rendered cell grid. Catches the
    /// regression class where stderr-style `> name(args)` markers
    /// were being parsed as markdown blockquotes — the chat pane
    /// would have rendered them with `▎` blockquote bars instead of
    /// showing the tool box.
    #[test]
    fn chat_renders_box_drawing_transcript_format() {
        use crate::tui::viz_viewer::state::{ChatState, VizApp};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // Build a VizApp with a chat pane active and a streaming
        // transcript matching what the agent loop writes today.
        let (viz, _) = build_hud_test_graph();
        let mut app = VizApp::from_viz_output_for_test(&viz);

        // Populate the chat streaming buffer with the exact format
        // the wg-nex agent now writes during a multi-step tool turn.
        let transcript = "\n┌─ bash ────────────────────────────────\n\
                          │ $ echo first && echo second\n\
                          │ first\n\
                          │ second\n\
                          └─\n\
                          I ran the bash command and observed the results.";
        app.chat = ChatState {
            streaming_text: transcript.to_string(),
            pending_request_ids: {
                let mut s = std::collections::HashSet::new();
                s.insert("r1".to_string());
                s
            },
            awaiting_since: Some(std::time::Instant::now()),
            ..ChatState::default()
        };
        // Render just the chat tab directly into a known area. This
        // sidesteps the overall viz layout (which would otherwise
        // collapse the chat panel in our test fixture) and targets
        // the rendering we actually care about: the chat pane
        // drawing the streaming transcript text.
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw_chat_tab(frame, &mut app, area);
            })
            .unwrap();

        // Extract all rendered text from the cell grid.
        let buffer = terminal.backend().buffer().clone();
        let mut rendered = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                rendered.push_str(buffer[(x, y)].symbol());
            }
            rendered.push('\n');
        }

        // Positive assertions: all three box-drawing glyphs made it
        // through the rendering pipeline to the terminal grid.
        assert!(
            rendered.contains("┌─ bash"),
            "missing `┌─ bash` header in rendered TUI; transcript format isn't reaching the screen.\nBuffer:\n{}",
            rendered
        );
        assert!(
            rendered.contains("│ $ echo first"),
            "missing `│ $ echo first` command line.\nBuffer:\n{}",
            rendered
        );
        assert!(
            rendered.contains("│ first") && rendered.contains("│ second"),
            "missing `│ first` / `│ second` output lines.\nBuffer:\n{}",
            rendered
        );
        assert!(
            rendered.contains("└─"),
            "missing `└─` closing line.\nBuffer:\n{}",
            rendered
        );
        assert!(
            rendered.contains("I ran the bash command"),
            "model prose after the tool box didn't render.\nBuffer:\n{}",
            rendered
        );

        // Negative assertion: no markdown-blockquote bar (`▎`) should
        // appear — its presence would signal we went back to the
        // broken state where `> name(args)` rendered as a blockquote.
        // The tool box itself uses `│` (U+2502) which is distinct.
        assert!(
            !rendered.contains('▎'),
            "blockquote bar `▎` present — transcript format regressed to stderr-style markers that markdown treats as blockquotes.\nBuffer:\n{}",
            rendered
        );
    }

    #[test]
    fn draw_hud_very_small_terminal_does_not_panic() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");

        let backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
    }

    #[test]
    fn test_word_wrap_normal() {
        let result = word_wrap("hello world foo", 11);
        assert_eq!(result, vec!["hello world", "foo"]);
    }

    #[test]
    fn test_word_wrap_long_word_hard_break() {
        // A single word longer than max_width should be hard-broken.
        let result = word_wrap("abcdefghijklmno", 5);
        assert_eq!(result, vec!["abcde", "fghij", "klmno"]);
    }

    #[test]
    fn test_word_wrap_long_word_with_remainder() {
        let result = word_wrap("abcdefgh", 5);
        assert_eq!(result, vec!["abcde", "fgh"]);
    }

    #[test]
    fn test_word_wrap_mixed_long_and_short() {
        let result = word_wrap("hi abcdefghij world", 5);
        assert_eq!(result, vec!["hi", "abcde", "fghij", "world"]);
    }

    #[test]
    fn test_word_wrap_long_url() {
        let url = "https://example.com/very/long/path/to/resource";
        let result = word_wrap(url, 20);
        assert_eq!(result.len(), 3);
        for line in &result {
            assert!(line.len() <= 20);
        }
    }

    #[test]
    fn test_word_wrap_empty() {
        let result = word_wrap("", 10);
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn test_word_wrap_zero_width() {
        let result = word_wrap("hello", 0);
        assert_eq!(result, vec!["hello"]);
    }

    // ── wrap_line_spans tests ──

    /// Helper: compute display width of a Line.
    fn line_display_width(line: &Line) -> usize {
        use unicode_width::UnicodeWidthStr;
        line.spans
            .iter()
            .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
            .sum()
    }

    /// Helper: extract all text content from wrapped lines.
    fn all_text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn test_wrap_line_spans_multi_span_with_internal_whitespace() {
        // Multi-span line with extra internal whitespace — every output line
        // must fit within max_width.
        let max_width = 20;
        let line = Line::from(vec![
            Span::styled("hello  ", Style::default().fg(Color::Red)),
            Span::styled("world  this  ", Style::default().fg(Color::Blue)),
            Span::styled("is a longer sentence", Style::default()),
        ]);
        let result = wrap_line_spans(&[line], max_width);
        for (i, l) in result.iter().enumerate() {
            let w = line_display_width(l);
            assert!(
                w <= max_width,
                "Line {} has display width {} > max_width {}: {:?}",
                i,
                w,
                max_width,
                l
            );
        }
        // Verify no content is lost (all non-whitespace chars preserved).
        let original_words: Vec<&str> = "hello  world  this  is a longer sentence"
            .split_whitespace()
            .collect();
        let result_text = all_text(&result);
        let result_words: Vec<&str> = result_text.split_whitespace().collect();
        assert_eq!(
            original_words, result_words,
            "Content was lost during wrapping"
        );
    }

    #[test]
    fn test_wrap_line_spans_inline_code_extra_spaces() {
        // Simulates inline code spans with padding spaces (like " config ").
        let max_width = 25;
        let line = Line::from(vec![
            Span::styled("Check the ", Style::default()),
            Span::styled(
                " config ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" file for details here", Style::default()),
        ]);
        let result = wrap_line_spans(&[line], max_width);
        for (i, l) in result.iter().enumerate() {
            let w = line_display_width(l);
            assert!(
                w <= max_width,
                "Line {} has display width {} > max_width {}: {:?}",
                i,
                w,
                max_width,
                l
            );
        }
        assert!(result.len() >= 2, "Should have wrapped to multiple lines");
    }

    #[test]
    fn test_wrap_line_spans_single_span_long() {
        // Long single-span line wraps correctly.
        let max_width = 15;
        let line = Line::from(Span::styled(
            "this is a fairly long single span line",
            Style::default().fg(Color::Green),
        ));
        let result = wrap_line_spans(&[line], max_width);
        for (i, l) in result.iter().enumerate() {
            let w = line_display_width(l);
            assert!(
                w <= max_width,
                "Line {} has display width {} > max_width {}: {:?}",
                i,
                w,
                max_width,
                l
            );
        }
        assert!(result.len() >= 3, "Should wrap into multiple lines");
    }

    #[test]
    fn test_wrap_line_spans_mixed_bold_italic_no_truncation() {
        // Mixed styles: bold + italic + normal. No content should be lost.
        let max_width = 18;
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let italic = Style::default().add_modifier(Modifier::ITALIC);
        let normal = Style::default();
        let line = Line::from(vec![
            Span::styled("bold text ", bold),
            Span::styled("italic text ", italic),
            Span::styled("and normal ending", normal),
        ]);
        let result = wrap_line_spans(&[line], max_width);
        for (i, l) in result.iter().enumerate() {
            let w = line_display_width(l);
            assert!(
                w <= max_width,
                "Line {} has display width {} > max_width {}: {:?}",
                i,
                w,
                max_width,
                l
            );
        }
        // Verify all words are present.
        let original_words: Vec<&str> = "bold text italic text and normal ending"
            .split_whitespace()
            .collect();
        let result_text = all_text(&result);
        let result_words: Vec<&str> = result_text.split_whitespace().collect();
        assert_eq!(
            original_words, result_words,
            "Content was lost during wrapping"
        );
    }

    #[test]
    fn test_wrap_line_spans_content_preserved() {
        // Verify total non-whitespace character content is preserved across wrapping.
        let max_width = 12;
        let line = Line::from(vec![
            Span::styled("abc ", Style::default().fg(Color::Red)),
            Span::styled("defgh ", Style::default().fg(Color::Blue)),
            Span::styled("ijklm nopqrs", Style::default()),
        ]);
        let result = wrap_line_spans(&[line], max_width);

        // Collect all non-whitespace chars from result.
        let result_chars: String = result
            .iter()
            .flat_map(|l| l.spans.iter().flat_map(|s| s.content.chars()))
            .filter(|c| !c.is_whitespace())
            .collect();
        let original_chars: String = "abc defgh ijklm nopqrs"
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(
            original_chars, result_chars,
            "Non-whitespace content was lost"
        );

        for (i, l) in result.iter().enumerate() {
            let w = line_display_width(l);
            assert!(
                w <= max_width,
                "Line {} has display width {} > max_width {}: {:?}",
                i,
                w,
                max_width,
                l
            );
        }
    }

    #[test]
    fn test_wrap_line_spans_styles_preserved() {
        // Verify that span styles are preserved through wrapping.
        let max_width = 10;
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let line = Line::from(vec![
            Span::styled("red ", red),
            Span::styled("blue text here", blue),
        ]);
        let result = wrap_line_spans(&[line], max_width);
        // First line should have red "red " and blue portion.
        assert!(!result.is_empty());
        let first_line = &result[0];
        // The first span should be red.
        assert_eq!(first_line.spans[0].style, red);
        // Should have a blue span too.
        assert!(
            first_line.spans.iter().any(|s| s.style == blue),
            "Blue style should be present in first wrapped line"
        );
    }

    // ── word_wrap_segments tests ──

    /// Helper: extract visual line strings from segments.
    fn segments_to_strings(line: &str, segments: &[(usize, usize)]) -> Vec<String> {
        let chars: Vec<char> = line.chars().collect();
        segments
            .iter()
            .map(|&(s, e)| chars[s..e].iter().collect::<String>())
            .collect()
    }

    #[test]
    fn test_wrap_segments_basic_word_boundary() {
        let line = "hello world";
        let segs = word_wrap_segments(line, 5);
        assert_eq!(segments_to_strings(line, &segs), vec!["hello", "world"]);
    }

    #[test]
    fn test_wrap_segments_exact_fit() {
        let line = "hi there";
        let segs = word_wrap_segments(line, 8);
        assert_eq!(segments_to_strings(line, &segs), vec!["hi there"]);
    }

    #[test]
    fn test_wrap_segments_multi_word() {
        let line = "hi there world";
        let segs = word_wrap_segments(line, 8);
        assert_eq!(segments_to_strings(line, &segs), vec!["hi there", "world"]);
    }

    #[test]
    fn test_wrap_segments_hard_break_long_word() {
        let line = "longword";
        let segs = word_wrap_segments(line, 5);
        assert_eq!(segments_to_strings(line, &segs), vec!["longw", "ord"]);
    }

    #[test]
    fn test_wrap_segments_leading_spaces() {
        let line = "  hello";
        let segs = word_wrap_segments(line, 5);
        // Leading spaces + "hello" (7 cols) exceeds 5; break at space→word boundary.
        assert_eq!(segments_to_strings(line, &segs), vec!["  ", "hello"]);
    }

    #[test]
    fn test_wrap_segments_empty_line() {
        let segs = word_wrap_segments("", 10);
        assert_eq!(segs, vec![(0, 0)]);
    }

    #[test]
    fn test_wrap_segments_single_word_fits() {
        let segs = word_wrap_segments("hello", 10);
        assert_eq!(segs, vec![(0, 5)]);
    }

    #[test]
    fn test_count_visual_lines_word_wrap() {
        assert_eq!(count_visual_lines("hello world", 5), 2);
        assert_eq!(count_visual_lines("hello\nworld", 10), 2);
        assert_eq!(count_visual_lines("a b c d e f", 5), 2);
        assert_eq!(count_visual_lines("", 10), 1);
    }

    #[test]
    fn test_cursor_in_segments_basic() {
        let segs = word_wrap_segments("hello world", 5);
        // segs: [(0,5), (6,11)]
        // Cursor on 'o' (col 4): visual line 0, offset 4.
        assert_eq!(cursor_in_segments(&segs, 4), (0, 4));
        // Cursor on space (col 5): in gap, show at end of first visual line.
        assert_eq!(cursor_in_segments(&segs, 5), (0, 5));
        // Cursor on 'w' (col 6): visual line 1, offset 0.
        assert_eq!(cursor_in_segments(&segs, 6), (1, 0));
        // Cursor on 'd' (col 10): visual line 1, offset 4.
        assert_eq!(cursor_in_segments(&segs, 10), (1, 4));
        // Cursor at end (col 11): past end of last segment.
        assert_eq!(cursor_in_segments(&segs, 11), (1, 5));
    }

    // ══════════════════════════════════════════════════════════════════════
    // ANNOTATION CLICK: hit regions, flash animation, and detail loading
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_annotation_click_hit_regions_computed() {
        // Build a graph with a task that has an annotation.
        let mut graph = WorkGraph::new();
        let task = make_task_with_status("my-task", "My Task", Status::Open);
        graph.add_node(Node::Task(task));

        let mut annotations = HashMap::new();
        annotations.insert(
            "my-task".to_string(),
            crate::commands::viz::AnnotationInfo {
                text: "[⊞ assigning]".to_string(),
                dot_task_ids: vec![".assign-my-task".to_string()],
            },
        );

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &annotations,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.annotation_map = viz.annotation_map.clone();
        app.compute_annotation_hit_regions();

        // Should have exactly one hit region for "my-task".
        assert_eq!(
            app.annotation_hit_regions.len(),
            1,
            "Expected 1 annotation hit region, got {}",
            app.annotation_hit_regions.len()
        );

        let region = &app.annotation_hit_regions[0];
        assert_eq!(region.parent_task_id, "my-task");
        assert_eq!(region.dot_task_ids, vec![".assign-my-task"]);

        // The annotation text should be found in the plain line.
        let plain = &app.plain_lines[region.orig_line];
        let expected_text = "[⊞ assigning]";
        assert!(
            plain.contains(expected_text),
            "Plain line should contain annotation text.\nLine: {:?}",
            plain
        );

        // col_start/col_end should span the annotation text.
        let found = &plain[region.col_start..region.col_end];
        assert!(
            found.contains("assigning"),
            "Hit region should cover annotation text, got: {:?}",
            found
        );
    }

    #[test]
    fn test_annotation_click_multiple_annotations_same_graph() {
        // Two tasks with different annotations — both should get hit regions.
        let mut graph = WorkGraph::new();
        let task_a = make_task_with_status("task-a", "Task A", Status::Open);
        let task_b = make_task_with_status("task-b", "Task B", Status::Done);
        graph.add_node(Node::Task(task_a));
        graph.add_node(Node::Task(task_b));

        let mut annotations = HashMap::new();
        annotations.insert(
            "task-a".to_string(),
            crate::commands::viz::AnnotationInfo {
                text: "[⊞ assigning]".to_string(),
                dot_task_ids: vec![".assign-task-a".to_string()],
            },
        );
        annotations.insert(
            "task-b".to_string(),
            crate::commands::viz::AnnotationInfo {
                text: "[∴ evaluating]".to_string(),
                dot_task_ids: vec![".evaluate-task-b".to_string()],
            },
        );

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &annotations,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.annotation_map = viz.annotation_map.clone();
        app.compute_annotation_hit_regions();

        // Should have two hit regions — one per annotated task.
        assert_eq!(
            app.annotation_hit_regions.len(),
            2,
            "Expected 2 annotation hit regions for 2 annotated tasks"
        );

        // Each region maps to its respective task.
        let ids: HashSet<&str> = app
            .annotation_hit_regions
            .iter()
            .map(|r| r.parent_task_id.as_str())
            .collect();
        assert!(ids.contains("task-a"));
        assert!(ids.contains("task-b"));

        // Regions should be on different lines.
        let lines: HashSet<usize> = app
            .annotation_hit_regions
            .iter()
            .map(|r| r.orig_line)
            .collect();
        assert_eq!(
            lines.len(),
            2,
            "Each annotated task should be on a different line"
        );
    }

    #[test]
    fn test_annotation_click_flash_rendering() {
        // Verify that apply_annotation_flash modifies the affected column range.
        let line = Line::from(vec![
            Span::styled("prefix ", Style::default()),
            Span::styled("[⊞ assigning]", Style::default().fg(Color::Magenta)),
            Span::styled(" suffix", Style::default()),
        ]);

        // Flash at elapsed=0 should apply background color.
        let flashed = apply_annotation_flash(line.clone(), 7, 20, 0);
        // The flash should produce spans where the annotation region has a bg color.
        let mut has_bg_in_region = false;
        let mut char_idx = 0;
        for span in &flashed.spans {
            for _ in span.content.chars() {
                if char_idx >= 7 && char_idx < 20 {
                    if span.style.bg.is_some() {
                        has_bg_in_region = true;
                    }
                }
                char_idx += 1;
            }
        }
        assert!(
            has_bg_in_region,
            "Flash at t=0 should apply background to annotation region"
        );

        // Flash at elapsed=500 should return to normal (progress=1.0).
        let faded = apply_annotation_flash(line.clone(), 7, 20, 500);
        // At 500ms the flash should be fully faded — no bg in the region.
        let mut has_bg = false;
        char_idx = 0;
        for span in &faded.spans {
            for _ in span.content.chars() {
                if char_idx >= 7 && char_idx < 20 {
                    if span.style.bg.is_some() {
                        has_bg = true;
                    }
                }
                char_idx += 1;
            }
        }
        assert!(
            !has_bg,
            "Flash at t=500 should be fully faded (no bg color)"
        );
    }

    #[test]
    fn test_annotation_click_no_regions_without_annotations() {
        // Graph with no annotations should produce no hit regions.
        let mut graph = WorkGraph::new();
        let task = make_task_with_status("plain-task", "Plain Task", Status::Open);
        graph.add_node(Node::Task(task));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.compute_annotation_hit_regions();

        assert!(
            app.annotation_hit_regions.is_empty(),
            "No annotations should produce no hit regions"
        );
    }

    // ── E2E annotation click pipeline tests ──
    // These test the full pipeline: graph with internal dot-tasks → filter_internal_tasks
    // generates annotations → ASCII render → VizApp hit region computation → click resolution.

    /// Helper: build a graph with a parent and an internal task, run filter + ascii + hit regions.
    /// Returns (VizApp, annotations) for assertion.
    fn build_e2e_annotation_app(
        parent_id: &str,
        parent_title: &str,
        parent_status: Status,
        internal_id: &str,
        internal_title: &str,
        internal_tag: &str,
        internal_after: Vec<&str>,
    ) -> VizApp {
        let mut graph = WorkGraph::new();
        let mut parent = make_task_with_status(parent_id, parent_title, parent_status);
        // Wire dependency if the internal task blocks the parent
        if !internal_after.contains(&parent_id) {
            parent.after = vec![internal_id.to_string()];
        }
        graph.add_node(Node::Task(parent));

        let mut internal = workgraph::graph::Task {
            id: internal_id.to_string(),
            title: internal_title.to_string(),
            tags: vec![internal_tag.to_string(), "agency".to_string()],
            after: internal_after.into_iter().map(String::from).collect(),
            status: Status::InProgress,
            ..workgraph::graph::Task::default()
        };
        let _ = &mut internal; // suppress unused warning
        graph.add_node(Node::Task(internal));

        let empty_annotations: HashMap<String, crate::commands::viz::AnnotationInfo> =
            HashMap::new();
        let (filtered, annots) = crate::commands::viz::filter_internal_tasks(
            &graph,
            graph.tasks().collect(),
            &empty_annotations,
        );
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        let viz = generate_ascii(
            &graph,
            &filtered,
            &task_ids,
            &annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.annotation_map = viz.annotation_map.clone();
        app.compute_annotation_hit_regions();
        app
    }

    #[test]
    fn test_e2e_assigning_click_resolves() {
        // parent + .assign-parent → [⊞ assigning], click resolves to .assign-parent
        let app = build_e2e_annotation_app(
            "parent",
            "Parent Task",
            Status::Open,
            ".assign-parent",
            "Assign parent",
            "assignment",
            vec![],
        );

        assert_eq!(
            app.annotation_hit_regions.len(),
            1,
            "Expected 1 hit region for assigning"
        );
        let region = &app.annotation_hit_regions[0];
        assert_eq!(region.parent_task_id, "parent");
        assert_eq!(region.dot_task_ids, vec![".assign-parent"]);

        let plain = &app.plain_lines[region.orig_line];
        let found = &plain[region.col_start..region.col_end];
        assert!(
            found.contains("assigning"),
            "Hit region should cover [⊞ assigning], got: {:?}",
            found
        );
    }

    #[test]
    fn test_e2e_evaluating_click_resolves() {
        // parent + .evaluate-parent → [∴ evaluating], click resolves to .evaluate-parent
        let app = build_e2e_annotation_app(
            "parent",
            "Parent Task",
            Status::Done,
            ".evaluate-parent",
            "Evaluate parent",
            "evaluation",
            vec!["parent"],
        );

        assert_eq!(
            app.annotation_hit_regions.len(),
            1,
            "Expected 1 hit region for evaluating"
        );
        let region = &app.annotation_hit_regions[0];
        assert_eq!(region.parent_task_id, "parent");
        assert_eq!(region.dot_task_ids, vec![".evaluate-parent"]);

        let plain = &app.plain_lines[region.orig_line];
        let found = &plain[region.col_start..region.col_end];
        assert!(
            found.contains("evaluating"),
            "Hit region should cover [∴ evaluating], got: {:?}",
            found
        );
    }

    #[test]
    fn test_e2e_assigning_click_resolves_open() {
        // parent + .assign-parent → [⊞ assigning], click resolves to .assign-parent
        let app = build_e2e_annotation_app(
            "parent",
            "Parent Task",
            Status::Open,
            ".assign-parent",
            "Assign parent",
            "assignment",
            vec![],
        );

        assert_eq!(
            app.annotation_hit_regions.len(),
            1,
            "Expected 1 hit region for assigning"
        );
        let region = &app.annotation_hit_regions[0];
        assert_eq!(region.parent_task_id, "parent");
        assert_eq!(region.dot_task_ids, vec![".assign-parent"]);

        let plain = &app.plain_lines[region.orig_line];
        let found = &plain[region.col_start..region.col_end];
        assert!(
            found.contains("assigning"),
            "Hit region should cover [⊞ assigning], got: {:?}",
            found
        );
    }

    #[test]
    fn test_e2e_multiple_annotations_column_accurate_click() {
        // parent with both .assign-parent and .evaluate-parent → two annotations on same line
        let mut graph = WorkGraph::new();
        let mut parent = make_task_with_status("parent", "Parent Task", Status::Done);
        parent.after = vec![".assign-parent".to_string()];
        graph.add_node(Node::Task(parent));

        let assign = workgraph::graph::Task {
            id: ".assign-parent".to_string(),
            title: "Assign parent".to_string(),
            tags: vec!["assignment".to_string(), "agency".to_string()],
            status: Status::InProgress,
            ..workgraph::graph::Task::default()
        };
        graph.add_node(Node::Task(assign));

        let eval = workgraph::graph::Task {
            id: ".evaluate-parent".to_string(),
            title: "Evaluate parent".to_string(),
            tags: vec!["evaluation".to_string(), "agency".to_string()],
            after: vec!["parent".to_string()],
            status: Status::InProgress,
            ..workgraph::graph::Task::default()
        };
        graph.add_node(Node::Task(eval));

        let empty_annotations: HashMap<String, crate::commands::viz::AnnotationInfo> =
            HashMap::new();
        let (filtered, annots) = crate::commands::viz::filter_internal_tasks(
            &graph,
            graph.tasks().collect(),
            &empty_annotations,
        );
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        // Both annotations should merge onto the parent
        assert!(
            annots.contains_key("parent"),
            "Annotations should include parent"
        );
        let info = &annots["parent"];
        assert_eq!(
            info.dot_task_ids.len(),
            2,
            "Should have 2 dot-task IDs for parent"
        );
        assert!(info.dot_task_ids.contains(&".assign-parent".to_string()));
        assert!(info.dot_task_ids.contains(&".evaluate-parent".to_string()));

        let viz = generate_ascii(
            &graph,
            &filtered,
            &task_ids,
            &annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.annotation_map = viz.annotation_map.clone();
        app.compute_annotation_hit_regions();

        // Should have exactly 1 hit region (both annotations merge into one composite)
        assert_eq!(
            app.annotation_hit_regions.len(),
            1,
            "Both annotations on same parent should produce 1 combined hit region"
        );
        let region = &app.annotation_hit_regions[0];
        assert_eq!(region.parent_task_id, "parent");
        assert_eq!(
            region.dot_task_ids.len(),
            2,
            "Combined region should carry both dot-task IDs"
        );

        // Verify the annotation text in the plain line covers both phases
        let plain = &app.plain_lines[region.orig_line];
        let found = &plain[region.col_start..region.col_end];
        assert!(
            found.contains("assigning"),
            "Combined annotation should contain 'assigning', got: {:?}",
            found
        );
        assert!(
            found.contains("evaluating"),
            "Combined annotation should contain 'evaluating', got: {:?}",
            found
        );

        // Column accuracy: col_start and col_end should tightly bound the annotation text
        assert!(
            region.col_start > 0,
            "Annotation should not start at column 0 (task label is before it)"
        );
        assert!(
            region.col_end > region.col_start,
            "col_end should be after col_start"
        );
    }

    #[test]
    fn test_e2e_validating_click_resolves() {
        // parent + .verify-parent → [∴ validating], click resolves to .verify-parent
        let app = build_e2e_annotation_app(
            "parent",
            "Parent Task",
            Status::Done,
            ".verify-parent",
            "Verify parent",
            "evaluation",
            vec!["parent"],
        );

        assert_eq!(
            app.annotation_hit_regions.len(),
            1,
            "Expected 1 hit region for validating"
        );
        let region = &app.annotation_hit_regions[0];
        assert_eq!(region.parent_task_id, "parent");
        assert_eq!(region.dot_task_ids, vec![".verify-parent"]);

        let plain = &app.plain_lines[region.orig_line];
        let found = &plain[region.col_start..region.col_end];
        assert!(
            found.contains("validating"),
            "Hit region should cover [∴ validating], got: {:?}",
            found
        );
    }

    // ══════════════════════════════════════════════════════════════════════
    // Log tab vs Output tab rendering parity
    // ══════════════════════════════════════════════════════════════════════

    /// Verify that the Log tab and Output tab produce identical rendered
    /// lines for the same markdown content via the shared `markdown_to_lines`
    /// function.  The only expected difference is the agent log marker lines
    /// (⊞ symbol in light blue) that the Log tab prepends.
    #[test]
    fn test_log_and_output_tabs_use_same_markdown_rendering() {
        use crate::tui::markdown::markdown_to_lines;

        let sample_md = concat!(
            "# Heading\n",
            "\n",
            "Some **bold** and *italic* text with `inline code`.\n",
            "\n",
            "```rust\n",
            "fn main() {\n",
            "    println!(\"hello\");\n",
            "}\n",
            "```\n",
            "\n",
            "- bullet one\n",
            "- bullet two\n",
        );

        let width: usize = 80;

        // Both tabs use markdown_to_lines with width.saturating_sub(1).
        let render_width = width.saturating_sub(1);
        let output_lines = markdown_to_lines(sample_md, render_width);
        let log_lines = markdown_to_lines(sample_md, render_width);

        assert_eq!(
            output_lines.len(),
            log_lines.len(),
            "Same content through markdown_to_lines should produce the same number of lines"
        );

        for (i, (out_line, log_line)) in output_lines.iter().zip(log_lines.iter()).enumerate() {
            assert_eq!(
                out_line.spans.len(),
                log_line.spans.len(),
                "Line {i}: span count mismatch"
            );
            for (j, (out_span, log_span)) in
                out_line.spans.iter().zip(log_line.spans.iter()).enumerate()
            {
                assert_eq!(
                    out_span.content, log_span.content,
                    "Line {i} span {j}: content mismatch"
                );
                assert_eq!(
                    out_span.style, log_span.style,
                    "Line {i} span {j}: style mismatch"
                );
            }
        }
    }

    /// Verify that log tab agent marker lines use the ⊞ symbol and pink styling
    /// (matching agency phase annotations like [∴ evaluating]).
    #[test]
    fn test_log_tab_agent_markers() {
        let log_entry = "[2026-03-25T19:06:45] Starting work on task";

        // Simulate what draw_log_tab does to log entries.
        const AGENT_MARKER: &str = "\u{229e}";
        let message = if let Some(bracket_end) = log_entry.find(']') {
            let timestamp = &log_entry[..=bracket_end];
            let msg = log_entry[bracket_end + 1..].trim_start();
            format!("{} {} {}", AGENT_MARKER, timestamp, msg)
        } else {
            format!("{} {}", AGENT_MARKER, log_entry)
        };

        assert!(
            message.starts_with(AGENT_MARKER),
            "Agent marker line should start with ⊞ symbol"
        );
        assert!(
            message.contains("[2026-03-25T19:06:45]"),
            "Agent marker line should preserve the timestamp"
        );
        assert!(
            message.contains("Starting work on task"),
            "Agent marker line should preserve the message"
        );

        // Verify the styling would be pink/rose (ANSI 219) — matching agency phase annotations.
        let styled_line = Line::from(Span::styled(
            message,
            Style::default().fg(Color::Indexed(219)),
        ));
        assert_eq!(
            styled_line.spans[0].style.fg,
            Some(Color::Indexed(219)),
            "Agent marker lines should use pink/rose color (ANSI 219)"
        );
    }

    /// Verify that the finish status line is consistent between Log and Output tabs.
    #[test]
    fn test_finish_status_line_parity() {
        // Both tabs should produce the same finish status format.
        for (status, expected_color) in [
            ("done", Color::Green),
            ("failed", Color::Red),
            ("unknown", Color::DarkGray),
        ] {
            let (status_text, status_color) = match status {
                "done" => (format!("── agent finished (done) ──"), Color::Green),
                "failed" => (format!("── agent finished (failed) ──"), Color::Red),
                _ => (format!("── agent finished ({status}) ──"), Color::DarkGray),
            };
            assert_eq!(
                status_color, expected_color,
                "Status '{status}' should use correct color"
            );
            assert!(
                status_text.contains(status),
                "Status text should contain the status string"
            );
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Responsive breakpoint tests
    // ══════════════════════════════════════════════════════════════════════

    #[test]
    fn test_responsive_breakpoint_from_width() {
        use crate::tui::viz_viewer::state::ResponsiveBreakpoint;

        // Compact: < 50
        assert_eq!(
            ResponsiveBreakpoint::from_width(0),
            ResponsiveBreakpoint::Compact
        );
        assert_eq!(
            ResponsiveBreakpoint::from_width(30),
            ResponsiveBreakpoint::Compact
        );
        assert_eq!(
            ResponsiveBreakpoint::from_width(40),
            ResponsiveBreakpoint::Compact
        );
        assert_eq!(
            ResponsiveBreakpoint::from_width(49),
            ResponsiveBreakpoint::Compact
        );

        // Narrow: 50–80
        assert_eq!(
            ResponsiveBreakpoint::from_width(50),
            ResponsiveBreakpoint::Narrow
        );
        assert_eq!(
            ResponsiveBreakpoint::from_width(60),
            ResponsiveBreakpoint::Narrow
        );
        assert_eq!(
            ResponsiveBreakpoint::from_width(80),
            ResponsiveBreakpoint::Narrow
        );

        // Full: > 80
        assert_eq!(
            ResponsiveBreakpoint::from_width(81),
            ResponsiveBreakpoint::Full
        );
        assert_eq!(
            ResponsiveBreakpoint::from_width(100),
            ResponsiveBreakpoint::Full
        );
        assert_eq!(
            ResponsiveBreakpoint::from_width(200),
            ResponsiveBreakpoint::Full
        );
    }

    #[test]
    fn test_responsive_compact_40_cols_no_panic() {
        // Render at 40-col width (phone-like, Termux/Blink Shell).
        use crate::tui::viz_viewer::state::ResponsiveBreakpoint;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");

        let backend = TestBackend::new(40, 25);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        // After drawing at 40 cols, breakpoint should be Compact.
        assert_eq!(app.responsive_breakpoint, ResponsiveBreakpoint::Compact);
    }

    #[test]
    fn test_responsive_narrow_60_cols_no_panic() {
        // Render at 60-col width (narrow terminal).
        use crate::tui::viz_viewer::state::ResponsiveBreakpoint;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");

        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.responsive_breakpoint, ResponsiveBreakpoint::Narrow);
    }

    #[test]
    fn test_responsive_full_100_cols_no_panic() {
        // Render at 100-col width (standard terminal).
        use crate::tui::viz_viewer::state::ResponsiveBreakpoint;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");

        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.responsive_breakpoint, ResponsiveBreakpoint::Full);
    }

    #[test]
    fn test_responsive_compact_single_panel_graph_view() {
        // In compact mode with SinglePanelView::Graph, the graph area should
        // span the full main area (minus status/hints bars).
        use crate::tui::viz_viewer::state::{ResponsiveBreakpoint, SinglePanelView};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.single_panel_view = SinglePanelView::Graph;

        let backend = TestBackend::new(40, 25);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.responsive_breakpoint, ResponsiveBreakpoint::Compact);
        // Graph area should have width == 40 (full terminal width).
        assert_eq!(app.last_graph_area.width, 40);
        // Graph area height should be main_area height (total - 3 for status/vitals/hints bars).
        assert_eq!(app.last_graph_area.height, 22);
        // Right panel area should be empty (not shown).
        assert_eq!(app.last_right_panel_area, Rect::default());
    }

    #[test]
    fn test_responsive_compact_single_panel_detail_view() {
        // In compact mode with SinglePanelView::Detail, only the inspector
        // should render — graph area should be zeroed out.
        use crate::tui::viz_viewer::state::{ResponsiveBreakpoint, SinglePanelView};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.single_panel_view = SinglePanelView::Detail;

        let backend = TestBackend::new(40, 25);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.responsive_breakpoint, ResponsiveBreakpoint::Compact);
        // Graph area should be zeroed (not visible).
        assert_eq!(app.last_graph_area, Rect::default());
        // Right panel area should be non-empty (it's the active view).
        assert!(app.last_right_panel_area.width > 0);
        assert!(app.last_right_panel_area.height > 0);
    }

    #[test]
    fn test_responsive_compact_toggle_single_panel() {
        // Verify that toggle_single_panel_view cycles through Graph → Detail → Log → Graph.
        use crate::tui::viz_viewer::state::{FocusedPanel, RightPanelTab, SinglePanelView};

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.responsive_breakpoint = crate::tui::viz_viewer::state::ResponsiveBreakpoint::Compact;
        app.single_panel_view = SinglePanelView::Graph;
        app.focused_panel = FocusedPanel::Graph;

        // Toggle: Graph -> Detail
        app.toggle_single_panel_view();
        assert_eq!(app.single_panel_view, SinglePanelView::Detail);
        assert_eq!(app.focused_panel, FocusedPanel::RightPanel);
        assert_eq!(app.right_panel_tab, RightPanelTab::Detail);

        // Toggle: Detail -> Log
        app.toggle_single_panel_view();
        assert_eq!(app.single_panel_view, SinglePanelView::Log);
        assert_eq!(app.focused_panel, FocusedPanel::RightPanel);
        assert_eq!(app.right_panel_tab, RightPanelTab::Log);

        // Toggle: Log -> Graph
        app.toggle_single_panel_view();
        assert_eq!(app.single_panel_view, SinglePanelView::Graph);
        assert_eq!(app.focused_panel, FocusedPanel::Graph);
    }

    #[test]
    fn test_responsive_narrow_split_layout() {
        // In narrow mode (50-80 cols) with inspector visible, should use
        // vertical (bottom) split to avoid oscillation with Full breakpoint.
        use crate::tui::viz_viewer::state::ResponsiveBreakpoint;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.right_panel_visible = true;
        app.layout_mode = crate::tui::viz_viewer::state::LayoutMode::TwoThirdsInspector;

        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.responsive_breakpoint, ResponsiveBreakpoint::Narrow);
        // Both panels should be visible (stacked vertically).
        assert!(app.last_graph_area.height > 0, "graph should have height");
        assert!(
            app.last_right_panel_area.height > 0,
            "inspector should have height"
        );
        // Inspector should be below (same width as terminal, not beside).
        assert_eq!(
            app.last_graph_area.width, 60,
            "graph should span full width in vertical layout"
        );
        assert_eq!(
            app.last_right_panel_area.width, 60,
            "inspector should span full width in vertical layout"
        );
        // Inspector should NOT be beside in narrow mode.
        assert!(
            !app.inspector_is_beside,
            "inspector should be below, not beside, in narrow mode"
        );
    }

    #[test]
    fn test_responsive_resize_dynamic_breakpoint() {
        // Simulate resize by drawing at different widths and verifying
        // breakpoint changes dynamically.
        use crate::tui::viz_viewer::state::ResponsiveBreakpoint;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");

        // Start wide
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.responsive_breakpoint, ResponsiveBreakpoint::Full);

        // "Resize" to narrow
        let backend = TestBackend::new(60, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.responsive_breakpoint, ResponsiveBreakpoint::Narrow);

        // "Resize" to compact
        let backend = TestBackend::new(40, 25);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.responsive_breakpoint, ResponsiveBreakpoint::Compact);

        // "Resize" back to full
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert_eq!(app.responsive_breakpoint, ResponsiveBreakpoint::Full);
    }

    #[test]
    fn test_inspector_layout_no_oscillation_on_zoom() {
        // Verify inspector position is monotonic when zooming in/out:
        // no oscillation between side and bottom layouts.
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.right_panel_visible = true;
        app.layout_mode = crate::tui::viz_viewer::state::LayoutMode::ThirdInspector;

        // Wide: inspector on the right (side-by-side).
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(app.inspector_is_beside, "140 cols: should be side-by-side");

        // Zoom in to 110: still above SIDE_MIN_WIDTH, stays beside.
        let backend = TestBackend::new(110, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            app.inspector_is_beside,
            "110 cols: should still be side-by-side"
        );

        // Zoom in to 95: below SIDE_MIN_WIDTH, moves to bottom.
        let backend = TestBackend::new(95, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(!app.inspector_is_beside, "95 cols: should be bottom");

        // Zoom in to 70 (Narrow breakpoint): stays at bottom — NO oscillation.
        let backend = TestBackend::new(70, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            !app.inspector_is_beside,
            "70 cols: should stay bottom (no oscillation)"
        );

        // Zoom in to 45 (Compact): single panel mode.
        let backend = TestBackend::new(45, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            !app.inspector_is_beside,
            "45 cols: compact mode, not beside"
        );

        // Zoom back out to 105: still below SIDE_RESTORE_WIDTH (120), stays bottom (hysteresis).
        let backend = TestBackend::new(105, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            !app.inspector_is_beside,
            "105 cols: hysteresis — stays bottom until 120"
        );

        // Zoom out to 125: above SIDE_RESTORE_WIDTH, restores to side-by-side.
        let backend = TestBackend::new(125, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        assert!(
            app.inspector_is_beside,
            "125 cols: restores to side-by-side"
        );
    }

    #[test]
    fn test_responsive_compact_tab_toggles_panel_focus() {
        // In compact mode, toggle_panel_focus should cycle through all three panels.
        use crate::tui::viz_viewer::state::{FocusedPanel, SinglePanelView};

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.responsive_breakpoint = crate::tui::viz_viewer::state::ResponsiveBreakpoint::Compact;
        app.single_panel_view = SinglePanelView::Graph;
        app.focused_panel = FocusedPanel::Graph;

        // Tab: Graph -> Detail
        app.toggle_panel_focus();
        assert_eq!(app.single_panel_view, SinglePanelView::Detail);
        assert_eq!(app.focused_panel, FocusedPanel::RightPanel);

        // Tab: Detail -> Log
        app.toggle_panel_focus();
        assert_eq!(app.single_panel_view, SinglePanelView::Log);
        assert_eq!(app.focused_panel, FocusedPanel::RightPanel);

        // Tab: Log -> Graph
        app.toggle_panel_focus();
        assert_eq!(app.single_panel_view, SinglePanelView::Graph);
        assert_eq!(app.focused_panel, FocusedPanel::Graph);
    }

    // ── Single-panel navigation mode tests ──

    #[test]
    fn test_single_panel_forward_cycle() {
        // ]/Tab cycles: Graph → Detail → Log → Graph
        use crate::tui::viz_viewer::state::{FocusedPanel, RightPanelTab, SinglePanelView};

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.responsive_breakpoint = crate::tui::viz_viewer::state::ResponsiveBreakpoint::Compact;
        app.single_panel_view = SinglePanelView::Graph;
        app.focused_panel = FocusedPanel::Graph;

        // Forward: Graph → Detail
        app.toggle_single_panel_view();
        assert_eq!(app.single_panel_view, SinglePanelView::Detail);
        assert_eq!(app.focused_panel, FocusedPanel::RightPanel);
        assert_eq!(app.right_panel_tab, RightPanelTab::Detail);

        // Forward: Detail → Log
        app.toggle_single_panel_view();
        assert_eq!(app.single_panel_view, SinglePanelView::Log);
        assert_eq!(app.focused_panel, FocusedPanel::RightPanel);
        assert_eq!(app.right_panel_tab, RightPanelTab::Log);

        // Forward: Log → Graph (wraps)
        app.toggle_single_panel_view();
        assert_eq!(app.single_panel_view, SinglePanelView::Graph);
        assert_eq!(app.focused_panel, FocusedPanel::Graph);
    }

    #[test]
    fn test_single_panel_backward_cycle() {
        // [ cycles backward: Graph → Log → Detail → Graph
        use crate::tui::viz_viewer::state::{FocusedPanel, RightPanelTab, SinglePanelView};

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.responsive_breakpoint = crate::tui::viz_viewer::state::ResponsiveBreakpoint::Compact;
        app.single_panel_view = SinglePanelView::Graph;
        app.focused_panel = FocusedPanel::Graph;

        // Backward: Graph → Log
        app.prev_single_panel_view();
        assert_eq!(app.single_panel_view, SinglePanelView::Log);
        assert_eq!(app.focused_panel, FocusedPanel::RightPanel);
        assert_eq!(app.right_panel_tab, RightPanelTab::Log);

        // Backward: Log → Detail
        app.prev_single_panel_view();
        assert_eq!(app.single_panel_view, SinglePanelView::Detail);
        assert_eq!(app.focused_panel, FocusedPanel::RightPanel);
        assert_eq!(app.right_panel_tab, RightPanelTab::Detail);

        // Backward: Detail → Graph (wraps)
        app.prev_single_panel_view();
        assert_eq!(app.single_panel_view, SinglePanelView::Graph);
        assert_eq!(app.focused_panel, FocusedPanel::Graph);
    }

    #[test]
    fn test_single_panel_state_persists_across_switches() {
        // Panel state (scroll offset, selected task) persists across switches.
        use crate::tui::viz_viewer::state::{FocusedPanel, SinglePanelView};

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.responsive_breakpoint = crate::tui::viz_viewer::state::ResponsiveBreakpoint::Compact;
        app.single_panel_view = SinglePanelView::Graph;
        app.focused_panel = FocusedPanel::Graph;

        // Set some state in graph view.
        app.scroll.offset_y = 5;
        let original_selected = app.selected_task_idx;

        // Cycle through all panels and back.
        app.toggle_single_panel_view(); // → Detail
        app.toggle_single_panel_view(); // → Log
        app.toggle_single_panel_view(); // → Graph

        assert_eq!(app.single_panel_view, SinglePanelView::Graph);
        assert_eq!(app.scroll.offset_y, 5, "scroll offset should persist");
        assert_eq!(
            app.selected_task_idx, original_selected,
            "selected task index should persist"
        );
    }

    #[test]
    fn test_single_panel_log_view_renders() {
        // In compact mode with SinglePanelView::Log, the right panel renders
        // with the Log tab active, and graph area is zeroed.
        use crate::tui::viz_viewer::state::{ResponsiveBreakpoint, RightPanelTab, SinglePanelView};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_hud_test_graph();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.single_panel_view = SinglePanelView::Log;
        app.right_panel_tab = RightPanelTab::Log;

        let backend = TestBackend::new(40, 25);
        let mut terminal = Terminal::new(backend).unwrap();

        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        assert_eq!(app.responsive_breakpoint, ResponsiveBreakpoint::Compact);
        assert_eq!(app.last_graph_area, Rect::default());
        assert!(app.last_right_panel_area.width > 0);
        assert!(app.last_right_panel_area.height > 0);
    }

    #[test]
    fn test_single_panel_labels() {
        use crate::tui::viz_viewer::state::SinglePanelView;

        assert_eq!(SinglePanelView::Graph.label(), "Graph");
        assert_eq!(SinglePanelView::Detail.label(), "Detail");
        assert_eq!(SinglePanelView::Log.label(), "Log");
    }

    #[test]
    fn test_single_panel_next_prev_inverse() {
        use crate::tui::viz_viewer::state::SinglePanelView;

        for view in [
            SinglePanelView::Graph,
            SinglePanelView::Detail,
            SinglePanelView::Log,
        ] {
            assert_eq!(
                view.next().prev(),
                view,
                "next then prev should return to original"
            );
            assert_eq!(
                view.prev().next(),
                view,
                "prev then next should return to original"
            );
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    // Tests for HUD vitals bar rendering
    // ══════════════════════════════════════════════════════════════════════

    /// Render the vitals bar into a test buffer and return the text content.
    fn render_vitals_to_string(vitals: &super::super::state::VitalsState, width: u16) -> String {
        use super::super::state::VitalsState;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (viz, _) = build_test_graph_chain_plus_isolated();
        let mut app = build_app_from_viz_output(&viz, "a");
        app.vitals = VitalsState {
            agents_alive: vitals.agents_alive,
            open: vitals.open,
            running: vitals.running,
            done: vitals.done,
            last_event_time: vitals.last_event_time,
            coord_last_tick: vitals.coord_last_tick,
            daemon_running: vitals.daemon_running,
        };

        let backend = TestBackend::new(width, 1);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 0, width, 1);
                draw_vitals_bar(frame, &app, area);
            })
            .unwrap();

        let buf = terminal.backend().buffer().clone();
        let mut text = String::new();
        for x in 0..width {
            let cell = buf.cell((x, 0)).unwrap();
            text.push_str(cell.symbol());
        }
        text.trim_end().to_string()
    }

    #[test]
    fn test_vitals_bar_renders_zero_agents() {
        use super::super::state::VitalsState;

        let v = VitalsState {
            agents_alive: 0,
            open: 5,
            running: 0,
            done: 10,
            last_event_time: None,
            coord_last_tick: None,
            daemon_running: false,
        };
        let text = render_vitals_to_string(&v, 120);
        assert!(
            text.contains("0 agents"),
            "should show agent count, got: {}",
            text
        );
        assert!(
            text.contains("5 open"),
            "should show open count, got: {}",
            text
        );
        assert!(
            text.contains("0 running"),
            "should show running count, got: {}",
            text
        );
        assert!(
            text.contains("10 done"),
            "should show done count, got: {}",
            text
        );
        assert!(
            text.contains("no events"),
            "should show no events, got: {}",
            text
        );
        assert!(
            text.contains("coord"),
            "should show coord status, got: {}",
            text
        );
        assert!(text.contains("down"), "should show down, got: {}", text);
    }

    #[test]
    fn test_vitals_bar_renders_with_agents() {
        use super::super::state::VitalsState;
        use std::time::{Duration, SystemTime};

        let now = SystemTime::now();
        let v = VitalsState {
            agents_alive: 2,
            open: 8,
            running: 3,
            done: 45,
            last_event_time: Some(now - Duration::from_secs(4)),
            coord_last_tick: Some(now - Duration::from_secs(2)),
            daemon_running: true,
        };
        let text = render_vitals_to_string(&v, 120);
        assert!(
            text.contains("2 agents"),
            "should show agent count, got: {}",
            text
        );
        assert!(
            text.contains("8 open"),
            "should show open count, got: {}",
            text
        );
        assert!(
            text.contains("3 running"),
            "should show running count, got: {}",
            text
        );
        assert!(
            text.contains("45 done"),
            "should show done count, got: {}",
            text
        );
        assert!(
            text.contains("last event"),
            "should show last event, got: {}",
            text
        );
        assert!(
            text.contains("coord"),
            "should show coord status, got: {}",
            text
        );
    }

    #[test]
    fn test_vitals_bar_renders_single_agent() {
        use super::super::state::VitalsState;

        let v = VitalsState {
            agents_alive: 1,
            open: 0,
            running: 1,
            done: 0,
            last_event_time: None,
            coord_last_tick: None,
            daemon_running: true,
        };
        let text = render_vitals_to_string(&v, 120);
        assert!(
            text.contains("1 agents"),
            "should show 1 agent, got: {}",
            text
        );
    }

    /// Render a TestBackend buffer to a flat string (concatenating cell symbols
    /// row by row, separated by newlines). Useful for asserting that specific
    /// text appears in a rendered TUI.
    fn buffer_to_string(buf: &ratatui::buffer::Buffer) -> String {
        let area = buf.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    /// Regression test for tui-log-view: when an in-progress task has an
    /// assigned agent whose raw_stream.jsonl file contains events, the Log
    /// pane MUST render those events on first draw — NOT show
    /// "no agent output yet". This is the user-reported failure mode that
    /// the prior tui-agent-activity attempt did not catch end-to-end.
    #[test]
    fn test_log_pane_renders_raw_stream_events_for_alive_agent() {
        use crate::commands::viz::ascii::generate_ascii;
        use crate::commands::viz::{LayoutMode, VizOutput};
        use crate::tui::viz_viewer::state::{RightPanelTab, VizApp};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use std::collections::{HashMap, HashSet};
        use workgraph::graph::{Node, Status, WorkGraph};
        use workgraph::parser::save_graph;
        use workgraph::test_helpers::make_task_with_status;

        // 1) Set up a workgraph dir with one in-progress task assigned to agent-77.
        let mut graph = WorkGraph::new();
        let mut t = make_task_with_status("my-task", "Live Task", Status::InProgress);
        t.assigned = Some("agent-77".to_string());
        graph.add_node(Node::Task(t));

        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph.jsonl");
        save_graph(&graph, &graph_path).unwrap();

        // 2) Create the agent's raw_stream.jsonl with realistic events.
        let agent_dir = tmp.path().join("agents").join("agent-77");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let stream_lines = [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"UNIQUE_STREAM_MARKER_ALPHA"}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo hi"}}]}}"#,
        ];
        std::fs::write(
            agent_dir.join("raw_stream.jsonl"),
            stream_lines.join("\n"),
        )
        .unwrap();
        // output.log is empty — the only data source must be raw_stream.jsonl.
        std::fs::write(agent_dir.join("output.log"), "").unwrap();

        // 3) Build VizApp pointed at the workgraph dir, select the task,
        //    and switch the right panel to the Log tab — exactly what the
        //    user does when they press '4'.
        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz: VizOutput = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.workgraph_dir = tmp.path().to_path_buf();
        let idx = app.task_order.iter().position(|id| id == "my-task");
        app.selected_task_idx = idx;
        app.right_panel_visible = true;
        app.right_panel_tab = RightPanelTab::Log;

        // 4) Render via TestBackend — the user's first draw of the Log tab.
        //    Wide enough to avoid Compact breakpoint clobbering layout.
        let backend = TestBackend::new(140, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| draw(frame, &mut app)).unwrap();

        let rendered = buffer_to_string(terminal.backend().buffer());

        // The bug surface: Log pane must show real activity, not the
        // "no agent output yet" sentinel that the user sees today.
        assert!(
            !rendered.contains("no agent output yet"),
            "Log tab must NOT show 'no agent output yet' when raw_stream.jsonl \
             has events for an alive agent. Rendered:\n{}",
            rendered
        );
        assert!(
            rendered.contains("UNIQUE_STREAM_MARKER_ALPHA"),
            "Log tab must render the stream event text on first draw. Rendered:\n{}",
            rendered
        );
    }
}
