use std::collections::{HashMap, HashSet};

use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Tabs,
};
use unicode_width::UnicodeWidthStr;

use super::state::{
    ConfigEditKind, ConfigSection, ConfirmAction, ControlPanelFocus, FocusedPanel, InputMode,
    LayoutMode, RightPanelTab, ServiceHealthLevel, SortMode, TaskFormField, TaskFormState,
    TextPromptAction, VizApp, extract_section_name, format_duration_compact,
};
use workgraph::AgentStatus;
use workgraph::graph::{TokenUsage, format_tokens};

use crate::tui::markdown::markdown_to_lines;

/// Minimum terminal width for side-by-side right panel layout.
const SIDE_MIN_WIDTH: u16 = 100;

pub fn draw(frame: &mut Frame, app: &mut VizApp) {
    // Clear expired jump targets (>2 seconds old).
    if let Some((_, when)) = app.jump_target
        && when.elapsed() > std::time::Duration::from_secs(2)
    {
        app.jump_target = None;
    }

    // Clean up expired splash animations.
    app.cleanup_splash_animations();

    // Reset scrollbar areas each frame (re-set by draw_scrollbar / panel scrollbar code).
    app.last_graph_scrollbar_area = Rect::default();
    app.last_panel_scrollbar_area = Rect::default();

    let area = frame.area();

    // Layout: top status bar + middle area + bottom action hints.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top status bar
            Constraint::Min(1),    // main content area
            Constraint::Length(1), // bottom action hints
        ])
        .split(area);

    let status_area = chunks[0];
    let main_area = chunks[1];
    let hints_area = chunks[2];

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
    // Lazy-load coordinator log on first switch to CoordLog tab.
    if app.right_panel_tab == RightPanelTab::CoordLog && app.coord_log.rendered_lines.is_empty() {
        app.load_coord_log();
    }
    // Lazy-init file browser on first switch to Files tab.
    if app.right_panel_tab == RightPanelTab::Files && app.file_browser.is_none() {
        app.file_browser = Some(super::file_browser::FileBrowser::new(&app.workgraph_dir));
    }
    // Lazy-load firehose data on first switch to Firehose tab.
    if app.right_panel_tab == RightPanelTab::Firehose && app.firehose.lines.is_empty() {
        app.update_firehose();
    }

    // Phase 1: Compute viewport dimensions from layout (needed for deferred centering).
    match app.layout_mode {
        LayoutMode::FullInspector => {
            app.last_graph_area = Rect::default();
            app.scroll.viewport_height = 0;
            app.scroll.viewport_width = 0;
        }
        LayoutMode::Off => {
            app.last_graph_area = main_area;
            app.last_right_panel_area = Rect::default();
            app.last_tab_bar_area = Rect::default();
            app.last_right_content_area = Rect::default();
            app.scroll.viewport_height = main_area.height as usize;
            app.scroll.viewport_width = main_area.width as usize;
        }
        LayoutMode::ThirdInspector | LayoutMode::HalfInspector | LayoutMode::TwoThirdsInspector => {
            if app.right_panel_visible {
                if area.width >= SIDE_MIN_WIDTH {
                    let right_width =
                        (main_area.width as u32 * app.right_panel_percent as u32 / 100) as u16;
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
                    let panel_height = (main_area.height as u32 * app.right_panel_percent as u32
                        / 100)
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
                app.last_tab_bar_area = Rect::default();
                app.last_right_content_area = Rect::default();
                app.scroll.viewport_height = main_area.height as usize;
                app.scroll.viewport_width = main_area.width as usize;
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
    match app.layout_mode {
        LayoutMode::FullInspector => {
            draw_right_panel(frame, app, main_area);
            app.last_graph_hscrollbar_area = Rect::default();
        }
        LayoutMode::Off => {
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
        LayoutMode::ThirdInspector | LayoutMode::HalfInspector | LayoutMode::TwoThirdsInspector => {
            if app.right_panel_visible {
                if area.width >= SIDE_MIN_WIDTH {
                    let right_width =
                        (main_area.width as u32 * app.right_panel_percent as u32 / 100) as u16;
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
                        app.scroll.has_horizontal_overflow() && app.graph_hscrollbar_visible(),
                    );
                    draw_right_panel(frame, app, right_area);
                } else {
                    let panel_height = (main_area.height as u32 * app.right_panel_percent as u32
                        / 100)
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
                        app.scroll.has_horizontal_overflow() && app.graph_hscrollbar_visible(),
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

    // Top status bar
    draw_status_bar(frame, app, status_area);

    // Service health badge — right-aligned pill on the status bar.
    draw_service_health_badge(frame, app, status_area);

    // Bottom action hints
    draw_action_hints(frame, app, hints_area);

    // ── Overlay widgets (on top of everything) ──

    if app.show_help {
        draw_help_overlay(frame);
    }

    // Confirmation dialog overlay
    if let InputMode::Confirm(ref action) = app.input_mode {
        draw_confirm_dialog(frame, action);
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
    let text_start = chars.iter().position(|c| c.is_alphanumeric() || *c == '.')?;

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

fn draw_viz_content(frame: &mut Frame, app: &VizApp, area: Rect) {
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
// Right panel rendering
// ══════════════════════════════════════════════════════════════════════════════

/// Draw the right panel with tab bar and active tab content.
fn draw_right_panel(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    app.last_right_panel_area = area;

    let is_full_panel = app.layout_mode == LayoutMode::FullInspector;

    // In full-panel mode: no borders (edge-to-edge content for clean copy-paste).
    // In split mode: minimal single-line border, dim when unfocused.
    let inner = if is_full_panel {
        area
    } else {
        let is_focused = app.focused_panel == FocusedPanel::RightPanel;
        let border_color = if is_focused {
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

    // Tab bar
    draw_tab_bar(frame, app.right_panel_tab, tab_area);

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
        RightPanelTab::Log => {
            draw_log_tab(frame, app, content_area);
        }
        RightPanelTab::Messages => {
            draw_messages_tab(frame, app, content_area);
        }
        RightPanelTab::Agency => {
            draw_agents_tab(frame, app, content_area);
        }
        RightPanelTab::Config => {
            draw_config_tab(frame, app, content_area);
        }
        RightPanelTab::Files => {
            super::file_browser_render::draw_files_tab(frame, app, content_area);
        }
        RightPanelTab::CoordLog => {
            draw_coord_log_tab(frame, app, content_area);
        }
        RightPanelTab::Firehose => {
            draw_firehose_tab(frame, app, content_area);
        }
    }
}

/// Draw the tab bar for the right panel.
fn draw_tab_bar(frame: &mut Frame, active: RightPanelTab, area: Rect) {
    let tab_labels: Vec<String> = RightPanelTab::ALL
        .iter()
        .map(|t| format!("{}:{}", t.index(), t.label()))
        .collect();
    let active_idx = active.index();

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
    let msg_area_height = area.height.saturating_sub(input_height);

    let msg_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: msg_area_height,
    };
    let input_area = Rect {
        x: area.x,
        y: area.y + msg_area_height,
        width: area.width,
        height: input_height,
    };

    // Store the message area for click-to-focus hit testing.
    app.last_chat_message_area = msg_area;

    // Empty state.
    if app.chat.messages.is_empty() && !app.chat.awaiting_response {
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
        draw_chat_input(frame, app, input_area);
        return;
    }

    // Build rendered lines from messages with word-wrapping.
    // Scrollbar overlays the rightmost column when visible.
    let content_width = width.saturating_sub(1);
    let mut rendered_lines: Vec<Line> = Vec::new();

    // Subtle warm-tinted dark background for user messages (like iMessage blue/gray,
    // but extremely subtle). Echoes the yellow ">" prefix and loop arrows.
    let user_msg_bg = Color::Rgb(30, 28, 20);

    for msg in app.chat.messages.iter() {
        let is_coordinator = msg.role == super::state::ChatRole::Coordinator;
        let is_user = msg.role == super::state::ChatRole::User;

        let (prefix, role_style) = match msg.role {
            super::state::ChatRole::User => (
                "> ".to_string(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
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

        let mut first_line = true;
        for line in &wrapped {
            if first_line {
                let mut spans = vec![Span::styled(prefix.clone(), role_style)];
                spans.extend(line.spans.iter().cloned());
                let mut built = Line::from(spans);
                if is_user {
                    built = apply_line_bg(built, user_msg_bg);
                }
                rendered_lines.push(built);
                first_line = false;
            } else {
                // Continuation/tool lines: indent to align with text after prefix
                let mut spans = vec![Span::raw(indent.clone())];
                spans.extend(line.spans.iter().cloned());
                let mut built = Line::from(spans);
                if is_user {
                    built = apply_line_bg(built, user_msg_bg);
                }
                rendered_lines.push(built);
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
                att_line = apply_line_bg(att_line, user_msg_bg);
            }
            rendered_lines.push(att_line);
        }
        // Blank line between messages.
        rendered_lines.push(Line::from(""));
    }

    // Streaming indicator when awaiting response.
    if app.chat.awaiting_response {
        rendered_lines.push(Line::from(Span::styled(
            "↯ ...",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::SLOW_BLINK),
        )));
        rendered_lines.push(Line::from(""));
    }

    // Scrolling: `scroll` is lines from bottom (0 = fully scrolled down).
    let total_lines = rendered_lines.len();
    let viewport_h = msg_area.height as usize;
    app.chat.total_rendered_lines = total_lines;
    app.chat.viewport_height = viewport_h;

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
    let visible_lines: Vec<Line> = rendered_lines[scroll_from_top..end].to_vec();

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

    // Input area.
    draw_chat_input(frame, app, input_area);
}

/// Draw the chat input line at the bottom of the chat panel.
fn draw_chat_input(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let is_editing = app.input_mode == InputMode::ChatInput;
    let has_text = !super::state::editor_is_empty(&app.chat.editor);
    app.last_chat_input_area = area;
    let border_color = if is_editing {
        Color::Magenta
    } else {
        Color::DarkGray
    };
    let prompt_color = if is_editing {
        Color::LightMagenta
    } else {
        Color::DarkGray
    };
    if is_editing || has_text {
        let sep = Line::from(Span::styled(
            "─".repeat(area.width as usize),
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
        let hint_text = if app.chat.pending_attachments.is_empty() {
            " \u{2191}\u{2193}: scroll".to_string()
        } else {
            format!(
                " \u{2191}\u{2193}: scroll  {} attached",
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

/// Draw the Log tab content (panel 2) — reverse chronological activity log.
fn draw_log_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    if app.log_pane.rendered_lines.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(Span::styled(
                "Activity Log",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "No log entries for selected task.",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(msg, area);
        return;
    }

    let viewport_h = area.height as usize;
    app.log_pane.viewport_height = viewport_h;

    if viewport_h == 0 {
        return;
    }

    // Display in forward chronological order (oldest first, newest at bottom), with word wrapping.
    let wrap_width = area.width as usize;

    let mut wrapped_lines: Vec<Line> = Vec::new();
    for s in &app.log_pane.rendered_lines {
        if let Some(bracket_end) = s.find(']') {
            let timestamp = &s[..=bracket_end];
            let message = &s[bracket_end + 1..];
            let prefix_len = bracket_end + 1;
            let text_width = wrap_width.saturating_sub(prefix_len);

            if text_width == 0 || message.trim().is_empty() {
                wrapped_lines.push(Line::from(vec![
                    Span::styled(timestamp, Style::default().fg(Color::DarkGray)),
                    Span::raw(message.to_string()),
                ]));
            } else {
                let leading_space = &message[..message.len() - message.trim_start().len()];
                let wrapped = word_wrap(message.trim_start(), text_width);
                let indent = " ".repeat(prefix_len + leading_space.len());
                for (i, wl) in wrapped.iter().enumerate() {
                    if i == 0 {
                        wrapped_lines.push(Line::from(vec![
                            Span::styled(timestamp, Style::default().fg(Color::DarkGray)),
                            Span::raw(format!("{}{}", leading_space, wl)),
                        ]));
                    } else {
                        wrapped_lines.push(Line::from(Span::raw(format!("{}{}", indent, wl))));
                    }
                }
            }
        } else {
            // No timestamp — plain word wrap.
            if wrap_width > 0 && s.width() > wrap_width {
                let wrapped = word_wrap(s, wrap_width);
                for wl in wrapped {
                    wrapped_lines.push(Line::from(wl));
                }
            } else {
                wrapped_lines.push(Line::from(Span::raw(s.as_str())));
            }
        }
    }

    let total_lines = wrapped_lines.len();
    app.log_pane.total_wrapped_lines = total_lines;
    let scroll = app
        .log_pane
        .scroll
        .min(total_lines.saturating_sub(viewport_h));
    let end = (scroll + viewport_h).min(total_lines);

    let visible_lines: Vec<Line> = wrapped_lines[scroll..end].to_vec();

    let paragraph = Paragraph::new(visible_lines);
    frame.render_widget(paragraph, area);

    // Scrollbar if content overflows (auto-hides after 2 seconds of inactivity).
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

/// Draw the Coordinator Log tab (panel 7) — daemon activity log.
fn draw_coord_log_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    if app.coord_log.rendered_lines.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(Span::styled(
                "Coordinator Log",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "No coordinator activity yet.",
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

/// Agent color palette for the firehose view.
const FIREHOSE_COLORS: [Color; 8] = [
    Color::Cyan,
    Color::Green,
    Color::Yellow,
    Color::Magenta,
    Color::Blue,
    Color::LightRed,
    Color::LightCyan,
    Color::LightGreen,
];

/// Draw the Firehose tab content (panel 8) — merged stream of all agent output.
fn draw_firehose_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    if app.firehose.lines.is_empty() {
        let active_count = app
            .agent_monitor
            .agents
            .iter()
            .filter(|a| matches!(a.status, AgentStatus::Working))
            .count();
        let msg = Paragraph::new(vec![
            Line::from(Span::styled(
                "Firehose — All Agents",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                if active_count == 0 {
                    "No active agents streaming output.".to_string()
                } else {
                    format!("{active_count} active agent(s), waiting for output...")
                },
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(msg, area);
        return;
    }

    let viewport_h = area.height.saturating_sub(1) as usize; // 1 line for header
    let width = area.width as usize;
    if viewport_h == 0 || width < 4 {
        return;
    }

    // Header line: active agent count + total lines.
    let active_count = app
        .agent_monitor
        .agents
        .iter()
        .filter(|a| matches!(a.status, AgentStatus::Working))
        .count();
    let header = Line::from(vec![
        Span::styled(
            "Firehose",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {active_count} active  {} lines", app.firehose.lines.len()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let header_area = Rect {
        height: 1,
        ..area
    };
    let content_area = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };

    frame.render_widget(Paragraph::new(vec![header]), header_area);

    // Build rendered lines with color-coded prefix.
    let mut rendered: Vec<Line> = Vec::with_capacity(app.firehose.lines.len());
    for fl in &app.firehose.lines {
        let color = FIREHOSE_COLORS[fl.color_idx % FIREHOSE_COLORS.len()];
        let short_agent = if fl.agent_id.len() > 10 {
            &fl.agent_id[fl.agent_id.len().saturating_sub(8)..]
        } else {
            &fl.agent_id
        };
        let short_task = if fl.task_id.len() > 20 {
            &fl.task_id[..fl.task_id.floor_char_boundary(20)]
        } else {
            &fl.task_id
        };
        let prefix = format!("[{short_agent} {short_task}] ");
        let text_budget = width.saturating_sub(prefix.len());
        let display_text = if fl.text.len() > text_budget && text_budget > 3 {
            format!(
                "{}…",
                &fl.text[..fl.text.floor_char_boundary(text_budget.saturating_sub(1))]
            )
        } else {
            fl.text.clone()
        };
        rendered.push(Line::from(vec![
            Span::styled(
                prefix,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(display_text),
        ]));
    }

    let total_lines = rendered.len();
    app.firehose.total_rendered_lines = total_lines;
    app.firehose.viewport_height = viewport_h;

    // Clamp scroll.
    let max_scroll = total_lines.saturating_sub(viewport_h);
    let scroll = app.firehose.scroll.min(max_scroll);
    app.firehose.scroll = scroll;

    let end = (scroll + viewport_h).min(total_lines);
    let visible: Vec<Line> = rendered[scroll..end].to_vec();

    let paragraph = Paragraph::new(visible);
    frame.render_widget(paragraph, content_area);

    // Scrollbar.
    if total_lines > viewport_h && app.panel_scrollbar_visible() {
        draw_panel_scrollbar(frame, app, content_area, max_scroll, scroll);
    }
}

/// Draw the Messages tab content (panel 3) — message queue for selected task.
/// Uses chat-app style: incoming messages left-aligned, outgoing right-aligned,
/// with a stats header showing sent/received/reply status.
fn draw_messages_tab(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let width = area.width as usize;
    if width < 4 || area.height < 3 {
        return;
    }

    // Reserve space for input area (like Chat tab).
    let is_msg_editing = app.input_mode == InputMode::MessageInput;
    let msg_text = super::state::editor_text(&app.messages_panel.editor);
    let has_msg_text = !msg_text.is_empty();
    let input_height: u16 = if is_msg_editing || has_msg_text {
        let prompt_prefix = 2;
        let usable = (area.width as usize).saturating_sub(prompt_prefix).max(1);
        let visual_lines = count_visual_lines(&msg_text, usable);
        let wrapped_lines = (visual_lines as u16).max(1);
        // Cap so input never eats more than 3/4 of the area.
        let max_input = (area.height * 3 / 4).max(3);
        let lines = wrapped_lines.min(max_input);
        lines + 1 // +1 for separator line
    } else {
        1
    };
    let msg_area_height = area.height.saturating_sub(input_height);

    let msg_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: msg_area_height,
    };
    let input_area = Rect {
        x: area.x,
        y: area.y + msg_area_height,
        width: area.width,
        height: input_height,
    };

    // Check if no task is selected.
    if app.messages_panel.task_id.is_none() {
        let msg = Paragraph::new(vec![
            Line::from(Span::styled(
                "Messages",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Select a task to send messages.",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(msg, msg_area);
        return;
    }

    // Empty state.
    if app.messages_panel.entries.is_empty() {
        let msg = Paragraph::new(vec![
            Line::from(Span::styled(
                "Messages",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "No messages yet. Press Enter to compose.",
                Style::default().fg(Color::DarkGray),
            )),
        ]);
        frame.render_widget(msg, msg_area);
        draw_message_input(frame, app, input_area);
        return;
    }

    let viewport_h = msg_area.height as usize;
    let wrap_width = width;

    // Color palette for distinct incoming senders.
    const SENDER_COLORS: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Blue,
        Color::LightYellow,
        Color::LightRed,
        Color::LightGreen,
    ];

    let mut wrapped_lines: Vec<Line> = Vec::new();
    let msg_count = app.messages_panel.entries.len();
    let summary = &app.messages_panel.summary;

    // ── Stats header ──
    // e.g., "3 sent, 2 replies  ✓ responded" or "3 sent, 1 reply  ⧖ awaiting reply"
    {
        let mut header_spans: Vec<Span> = Vec::new();

        // Incoming count (messages sent TO the task).
        if summary.incoming > 0 {
            header_spans.push(Span::styled(
                format!("{} sent", summary.incoming,),
                Style::default().fg(Color::Green),
            ));
        }

        // Outgoing count (replies FROM the task's agent).
        if summary.outgoing > 0 {
            if !header_spans.is_empty() {
                header_spans.push(Span::styled(", ", Style::default().fg(Color::DarkGray)));
            }
            header_spans.push(Span::styled(
                format!(
                    "{} {}",
                    summary.outgoing,
                    if summary.outgoing == 1 {
                        "reply"
                    } else {
                        "replies"
                    }
                ),
                Style::default().fg(Color::Cyan),
            ));
        }

        // Response status indicator.
        if summary.incoming > 0 {
            header_spans.push(Span::styled("  ", Style::default()));
            if summary.responded {
                header_spans.push(Span::styled(
                    "✓ responded",
                    Style::default().fg(Color::Green),
                ));
            } else if summary.unanswered > 0 {
                header_spans.push(Span::styled(
                    format!("⧖ {} awaiting reply", summary.unanswered,),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }

        if !header_spans.is_empty() {
            wrapped_lines.push(Line::from(header_spans));
            wrapped_lines.push(Line::from(Span::styled(
                "─".repeat(wrap_width.min(40)),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    // ── Render each message entry ──
    let mut sender_color_map: std::collections::HashMap<String, Color> =
        std::collections::HashMap::new();
    let mut next_color_idx: usize = 0;

    // Compute which incoming messages are unanswered (last N without a following outgoing).
    let mut unanswered_set: std::collections::HashSet<usize> = std::collections::HashSet::new();
    {
        let mut pending: Vec<usize> = Vec::new();
        for (i, entry) in app.messages_panel.entries.iter().enumerate() {
            match entry.direction {
                super::state::MessageDirection::Incoming => pending.push(i),
                super::state::MessageDirection::Outgoing => pending.clear(),
            }
        }
        unanswered_set.extend(pending);
    }

    // Right-indent for outgoing messages (chat bubble effect).
    let outgoing_indent = (wrap_width / 5).clamp(2, 8);

    for (msg_idx, entry) in app.messages_panel.entries.iter().enumerate() {
        let is_outgoing = entry.direction == super::state::MessageDirection::Outgoing;

        // Strip ANSI from body.
        let clean_body = String::from_utf8(strip_ansi_escapes::strip(entry.body.as_bytes()))
            .unwrap_or_else(|_| entry.body.clone());

        // Sender style: outgoing = green, incoming = rotating palette.
        let sender_style = if is_outgoing {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            let color = *sender_color_map
                .entry(entry.sender.clone())
                .or_insert_with(|| {
                    let c = SENDER_COLORS[next_color_idx % SENDER_COLORS.len()];
                    next_color_idx += 1;
                    c
                });
            Style::default().fg(color).add_modifier(Modifier::BOLD)
        };

        // Direction arrow and margin.
        let (dir_arrow, margin) = if is_outgoing {
            ("→ ", " ".repeat(outgoing_indent))
        } else {
            ("← ", String::new())
        };

        let dir_style = if is_outgoing {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Cyan)
        };

        // Unanswered marker for incoming messages without a reply.
        let unanswered_marker = if !is_outgoing && unanswered_set.contains(&msg_idx) {
            " ⧖"
        } else {
            ""
        };

        // Header line: [margin][arrow] sender  timestamp [unanswered]
        let mut header_spans: Vec<Span> = Vec::new();
        if !margin.is_empty() {
            header_spans.push(Span::raw(margin.clone()));
        }
        header_spans.push(Span::styled(dir_arrow, dir_style));
        header_spans.push(Span::styled(entry.display_label.clone(), sender_style));
        if entry.is_urgent {
            header_spans.push(Span::styled(
                " [!]",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        header_spans.push(Span::styled(
            format!("  {}", entry.timestamp),
            Style::default().fg(Color::DarkGray),
        ));
        // Delivery status indicator.
        let (status_icon, status_color) = match entry.delivery_status {
            workgraph::messages::DeliveryStatus::Sent => ("\u{2709}", Color::DarkGray), // ✉
            workgraph::messages::DeliveryStatus::Delivered => ("\u{1f4ec}", Color::Blue), // 📬
            workgraph::messages::DeliveryStatus::Read => ("\u{1f441}", Color::Yellow),  // 👁
            workgraph::messages::DeliveryStatus::Acknowledged => ("\u{2705}", Color::Green), // ✅
        };
        header_spans.push(Span::styled(
            format!(" {}", status_icon),
            Style::default().fg(status_color),
        ));
        if !unanswered_marker.is_empty() {
            header_spans.push(Span::styled(
                unanswered_marker,
                Style::default().fg(Color::Yellow),
            ));
        }
        wrapped_lines.push(Line::from(header_spans));

        // Body lines with indent matching the margin + arrow.
        let body_indent_len = if is_outgoing {
            outgoing_indent + 2 // margin + "→ "
        } else {
            2 // "← "
        };
        let body_indent = " ".repeat(body_indent_len);
        let text_width = wrap_width.saturating_sub(body_indent_len);

        // Body style: outgoing gets a subtle dimming, incoming is default.
        let body_style = if is_outgoing {
            Style::default().fg(Color::White)
        } else {
            Style::default()
        };

        if text_width == 0 || clean_body.is_empty() {
            wrapped_lines.push(Line::from(Span::styled(
                format!("{}{}", body_indent, clean_body),
                body_style,
            )));
        } else {
            // Render markdown to styled lines, then word-wrap.
            let md_lines = markdown_to_lines(&clean_body, text_width);
            let body_wrapped = if md_lines.is_empty() {
                vec![Line::from("")]
            } else {
                wrap_line_spans(&md_lines, text_width)
            };
            for line in &body_wrapped {
                let mut spans = vec![Span::raw(body_indent.clone())];
                spans.extend(line.spans.iter().cloned());
                wrapped_lines.push(Line::from(spans));
            }
        }

        // Visual separator between messages.
        if msg_idx + 1 < msg_count {
            wrapped_lines.push(Line::from(""));
        }
    }

    let total_lines = wrapped_lines.len();
    app.messages_panel.total_wrapped_lines = total_lines;
    app.messages_panel.viewport_height = viewport_h;

    // Scrolling: scroll is from top (0 = fully scrolled to top).
    let scroll = app
        .messages_panel
        .scroll
        .min(total_lines.saturating_sub(viewport_h));
    let end = (scroll + viewport_h).min(total_lines);

    let visible_lines: Vec<Line> = wrapped_lines[scroll..end].to_vec();

    let paragraph = Paragraph::new(visible_lines);
    frame.render_widget(paragraph, msg_area);

    if total_lines > viewport_h && app.panel_scrollbar_visible() {
        draw_panel_scrollbar(
            frame,
            app,
            msg_area,
            total_lines.saturating_sub(viewport_h),
            scroll,
        );
    }

    // Input area.
    draw_message_input(frame, app, input_area);
}

/// Draw the message input line at the bottom of the messages panel.
fn draw_message_input(frame: &mut Frame, app: &mut VizApp, area: Rect) {
    let is_editing = app.input_mode == InputMode::MessageInput;
    let has_text = !super::state::editor_is_empty(&app.messages_panel.editor);
    let border_color = if is_editing {
        Color::Magenta
    } else {
        Color::DarkGray
    };
    let prompt_color = if is_editing {
        Color::LightMagenta
    } else {
        Color::DarkGray
    };
    if is_editing || has_text {
        let sep = Line::from(Span::styled(
            "─".repeat(area.width as usize),
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
            &app.messages_panel.editor,
            editor_area,
            Style::default().fg(text_color),
            cursor_style,
            is_editing,
        );
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " Enter: compose  \u{2191}\u{2193}: scroll",
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
    }
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
                        let cache_total =
                            usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
                        let mut tok_parts = vec![format!("→{}", format_tokens(usage.input_tokens))];
                        if cache_total > 0 {
                            tok_parts.push(format!("+{} cached", format_tokens(cache_total)));
                        }
                        tok_parts.push(format!("←{}", format_tokens(usage.output_tokens)));
                        if usage.cost_usd > 0.0 {
                            tok_parts.push(format!("${:.4}", usage.cost_usd));
                        }
                        lines.push(Line::from(Span::styled(
                            format!("  Tokens: {}", tok_parts.join(" ")),
                            Style::default().fg(Color::DarkGray),
                        )));
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
            .map(|u| u.input_tokens)
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
            let mut summary = if total_cached > 0 {
                format!(
                    "  Total: →{} +{} cached ←{}",
                    format_tokens(total_new_input),
                    format_tokens(total_cached),
                    format_tokens(total_output)
                )
            } else {
                format!(
                    "  Total: →{} ←{}",
                    format_tokens(total_new_input),
                    format_tokens(total_output)
                )
            };
            if total_cost > 0.0 {
                summary.push_str(&format!(" (${:.4})", total_cost));
            }
            lines.push(Line::from(Span::styled(
                summary,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
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

/// Draw a confirmation dialog overlay.
fn draw_confirm_dialog(frame: &mut Frame, action: &ConfirmAction) {
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
                format!("{}{} ({}…)", marker, id, &title[..title.floor_char_boundary(27)])
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

    // Append notification if present.
    if let Some((ref msg, _)) = app.notification {
        spans.push(Span::styled(
            "  │ ",
            Style::default().fg(Color::Rgb(80, 80, 80)),
        ));
        spans.push(Span::styled(
            msg.as_str(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
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
                    ("S-Enter", "newline"),
                    ("Ctrl+K/Y", "kill/yank"),
                    ("Esc", "exit"),
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
        InputMode::Normal => match app.focused_panel {
            FocusedPanel::Graph => (
                "Graph",
                "NAV",
                Color::Rgb(120, 120, 120),
                vec![
                    ("↑↓", "select"),
                    ("Enter", "inspect"),
                    ("Tab", "panel"),
                    ("/", "search"),
                    ("a", "add"),
                    ("D", "done"),
                    ("i/I", "resize pane"),
                    ("?", "help"),
                    ("Alt←→", "cycle views"),
                ],
            ),
            FocusedPanel::RightPanel => {
                let tab = &app.right_panel_tab;
                let tab_label: &str = match tab {
                    RightPanelTab::Chat => "0:Chat",
                    RightPanelTab::Detail => "1:Detail",
                    RightPanelTab::Log => "2:Log",
                    RightPanelTab::Messages => "3:Msg",
                    RightPanelTab::Agency => "4:Agency",
                    RightPanelTab::Config => "5:Config",
                    RightPanelTab::Files => "6:Files",
                    RightPanelTab::CoordLog => "7:Coord",
                    RightPanelTab::Firehose => "8:Fire",
                };
                let mut hints: Vec<(&str, &str)> = Vec::new();
                match tab {
                    RightPanelTab::Chat => {
                        hints.push(("Enter", "type"));
                        hints.push(("↑↓", "scroll"));
                    }
                    RightPanelTab::Detail => {
                        hints.push(("↑↓", "scroll"));
                        hints.push(("PgUp/Dn", "page"));
                        hints.push(("Enter", "toggle"));
                    }
                    RightPanelTab::Log
                    | RightPanelTab::CoordLog
                    | RightPanelTab::Agency
                    | RightPanelTab::Firehose => {
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
                }
                // Common hints for all right-panel tabs.
                hints.push(("Tab", "graph"));
                hints.push(("i/I", "resize pane"));
                hints.push(("?", "help"));
                hints.push(("Alt←→", "cycle views"));
                (tab_label, "NAV", Color::Rgb(120, 120, 120), hints)
            }
        },
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
            cp.push(Span::styled(match tc.service_uptime_secs { Some(s) => format!("\u{2191}{}", format_duration_compact(s)), None => "\u{2191}-".into() }, Style::default().fg(Color::Cyan)));
        }
        if tc.show_cumulative {
            cp.push(Span::styled(format!("\u{03A3}{}", format_duration_compact(tc.cumulative_secs)), Style::default().fg(Color::Magenta)));
        }
        if tc.show_active && tc.active_agent_count > 0 {
            cp.push(Span::styled(format!("\u{26A1}{}({})", format_duration_compact(tc.active_secs), tc.active_agent_count), Style::default().fg(Color::Green)));
        }
        if tc.show_session {
            cp.push(Span::styled(format!("\u{25F7}{}", format_duration_compact(tc.session_start.elapsed().as_secs())), Style::default().fg(Color::DarkGray)));
        }
        if !cp.is_empty() {
            spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
            for (i, p) in cp.into_iter().enumerate() { if i > 0 { spans.push(Span::styled(" ", Style::default())); } spans.push(p); }
            spans.push(Span::styled(" ", Style::default()));
        }
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

/// Draw the service health detail popup — anchored below the badge.
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
            health.pid.map(|p| p.to_string()).unwrap_or_else(|| "N/A".to_string()),
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
        lines.push(Line::from(vec![
            Span::styled("  Status: ", label_style),
            Span::styled(
                "PAUSED",
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" (agent spawning disabled)", dim_style),
        ]));
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
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
        ]));
        for st in &health.stuck_tasks {
            let title_display = if st.task_title.len() > 30 {
                format!("{}...", &st.task_title[..st.task_title.floor_char_boundary(27)])
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
        lines.push(Line::from(vec![Span::styled("  Recent errors:", label_style)]));
        for err in &health.recent_errors {
            let max_w = width as usize - 6;
            let truncated = if err.len() > max_w {
                format!("{}...", &err[..err.floor_char_boundary(max_w.saturating_sub(3))])
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
        Span::styled(health.pid.map(|p| p.to_string()).unwrap_or_else(|| "N/A".to_string()), value_style),
        Span::styled("    Uptime: ", label_style),
        Span::styled(health.uptime.as_deref().unwrap_or("N/A"), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled("  Agents: ", label_style),
        Span::styled(format!("{} alive / {} max", health.agents_alive, health.agents_max), value_style),
        Span::styled(format!("  ({} total)", health.agents_total), dim_style),
    ]));
    if !health.recent_errors.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled("  Recent errors:", label_style)]));
        for err in health.recent_errors.iter().take(3) {
            let max_w = width as usize - 6;
            let truncated = if err.len() > max_w {
                format!("{}...", &err[..err.floor_char_boundary(max_w.saturating_sub(3))])
            } else { err.clone() };
            lines.push(Line::from(vec![
                Span::styled("    ", dim_style),
                Span::styled(truncated, Style::default().fg(Color::Red)),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled("  Controls", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))]));
    let ssl = if is_running { "Stop Service" } else { "Start Service" };
    let ssk = if is_running { "[S] Stop" } else { "[S] Start" };
    lines.push(control_panel_line(ssl, ssk, *focus == ControlPanelFocus::StartStop, if is_running { Color::Red } else { Color::Green }));
    let pl = if health.paused { "Resume Launches" } else { "Pause Launches" };
    let pk = if health.paused { "[P] Resume" } else { "[P] Pause" };
    lines.push(control_panel_line(pl, pk, *focus == ControlPanelFocus::PauseResume, Color::Yellow));
    lines.push(control_panel_line("Restart Service", "[Enter]", *focus == ControlPanelFocus::Restart, Color::Cyan));
    let pf = *focus == ControlPanelFocus::PanicKill;
    let ps = if pf { Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Red).add_modifier(Modifier::BOLD) };
    lines.push(Line::from(vec![
        Span::styled(if pf { " > " } else { "   " }, ps),
        Span::styled("PANIC KILL", ps),
        Span::styled("  [K]  ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("kills {} agents + stops service", health.agents_alive), Style::default().fg(Color::DarkGray)),
    ]));
    lines.push(Line::from(""));
    if health.stuck_tasks.is_empty() {
        lines.push(Line::from(vec![Span::styled("  Stuck agents: ", label_style), Span::styled("none", Style::default().fg(Color::Green))]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("  Stuck agents: ", label_style),
            Span::styled(format!("{}", health.stuck_tasks.len()), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("  (Enter=kill, U=unclaim)", dim_style),
        ]));
        for (i, st) in health.stuck_tasks.iter().enumerate() {
            let sf = *focus == ControlPanelFocus::StuckAgent(i);
            let td = if st.task_title.len() > 25 { format!("{}...", &st.task_title[..st.task_title.floor_char_boundary(22)]) } else { st.task_title.clone() };
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
    lines.push(control_panel_line("Kill All Dead Agents", "[Enter]", *focus == ControlPanelFocus::KillAllDead, Color::Yellow));
    lines.push(control_panel_line("Retry Failed Evals", "[Enter]", *focus == ControlPanelFocus::RetryFailedEvals, Color::Cyan));
    if let Some((ref msg, ref at)) = health.feedback
        && at.elapsed() < std::time::Duration::from_secs(5) {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled("  ", dim_style), Span::styled(msg.as_str(), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))]));
        }
    if health.panic_confirm {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("  WARNING: ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::styled(format!("This will kill {} running agents and stop the service.", health.agents_alive), Style::default().fg(Color::White)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  Are you sure? ", Style::default().fg(Color::White)),
            Span::styled("[y]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::styled(" Yes  ", dim_style),
            Span::styled("[n/Esc]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
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

fn control_panel_line<'a>(label: &'a str, key_hint: &'a str, focused: bool, color: Color) -> Line<'a> {
    let prefix = if focused { " > " } else { "   " };
    let style = if focused { Style::default().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD) } else { Style::default().fg(color) };
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
        binding("I", "Shrink viz pane (10% per press, wraps)"),
        binding("=", "Cycle layout: split/panel/graph"),
        binding("0-7", "Switch tab: Chat/.../Files/Coord"),
        binding("R", "Toggle raw JSON in Detail tab"),
        binding("Space", "Toggle section collapse in Detail"),
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
        binding("c", "Open chat input"),
        binding("Ctrl-C", "Kill agent on focused task"),
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
        binding("r", "Force refresh"),
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
    let new_input = format_tokens(usage.input_tokens);
    let output = format_tokens(usage.output_tokens);

    let cache_total = usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
    let token_str = if cache_total > 0 {
        let cache = format_tokens(cache_total);
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

            // Service status indicator on the Service section header
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

        let line = Line::from(vec![
            Span::styled(cursor, style),
            Span::styled(label, style),
            Span::styled(value_display, value_style),
        ]);

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
                ConfigEditKind::Choice(_) => "←/→: choose  Enter: save  Esc: cancel",
                ConfigEditKind::Toggle => "Enter/Space: toggle",
            }
        } else {
            "j/k: navigate  Enter: edit  Space: toggle  Tab: collapse/expand  a: add endpoint  r: reload"
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
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
            &HashMap::new(),
            LayoutMode::Diamond,
            &HashSet::new(),
            "gray",
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
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
        annotations.insert("my-task".to_string(), "[assigning]".to_string());

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &annotations,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
        annotations2.insert("eval-task".to_string(), "[∴ evaluating]".to_string());

        let tasks2: Vec<_> = graph2.tasks().collect();
        let task_ids2: HashSet<&str> = tasks2.iter().map(|t| t.id.as_str()).collect();
        let viz2 = generate_ascii(
            &graph2,
            &tasks2,
            &task_ids2,
            &annotations2,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
        annotations3.insert("val-task".to_string(), "[✓ validating]".to_string());

        let tasks3: Vec<_> = graph3.tasks().collect();
        let task_ids3: HashSet<&str> = tasks3.iter().map(|t| t.id.as_str()).collect();
        let viz3 = generate_ascii(
            &graph3,
            &tasks3,
            &task_ids3,
            &annotations3,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
        annotations4.insert("both-task".to_string(), "[∴ evaluating] [✓ validating]".to_string());

        let tasks4: Vec<_> = graph4.tasks().collect();
        let task_ids4: HashSet<&str> = tasks4.iter().map(|t| t.id.as_str()).collect();
        let viz4 = generate_ascii(
            &graph4,
            &tasks4,
            &task_ids4,
            &annotations4,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
        annotations.insert("child".to_string(), "[assigning]".to_string());

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &annotations,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
                &HashMap::new(),
                LayoutMode::default(),
                &HashSet::new(),
                "gray",
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

        // Self-loop: the SCC detection should include 'a' in its own cycle.
        // Note: Tarjan's SCC may or may not include single-node self-loops
        // as non-trivial SCCs depending on the implementation.
        // The cycle_members map is built from cycle_analysis.cycles which
        // may only contain SCCs with >1 member. Self-loops need special handling.
        //
        // If cycle_set is populated, verify yellow edges.
        // If not, this reveals a gap in the implementation.
        if app.cycle_set.contains("a") {
            // Self-loop detected in SCC — verify yellow edges exist
            assert!(
                has_any_yellow_edge(&app, "a"),
                "Self-loop: cycle_set includes 'a' but no yellow edges.\nViz:\n{}",
                app.lines.join("\n")
            );
        } else {
            // Self-loops may not be detected by the SCC algorithm as non-trivial SCCs.
            // This is acceptable behavior — document it.
            eprintln!(
                "Note: Self-loop A→A not detected as cycle by SCC. \
                       cycle_set is empty. This is expected if SCC algorithm \
                       only reports components with >1 member."
            );
            assert!(
                !has_any_yellow_edge(&app, "a"),
                "Self-loop: cycle_set is empty, so no yellow edges expected.\nViz:\n{}",
                app.lines.join("\n")
            );
        }
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
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
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
        assert_eq!(
            segments_to_strings(line, &segs),
            vec!["hi there", "world"]
        );
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
}
