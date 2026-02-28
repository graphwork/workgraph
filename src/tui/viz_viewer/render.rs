use std::collections::HashSet;

use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use super::state::VizApp;
use workgraph::graph::{format_tokens, TokenUsage};

pub fn draw(frame: &mut Frame, app: &mut VizApp) {
    // Clear expired jump targets (>2 seconds old).
    if let Some((_, when)) = app.jump_target {
        if when.elapsed() > std::time::Duration::from_secs(2) {
            app.jump_target = None;
        }
    }

    let area = frame.area();

    // Layout: main content area + status bar (1 line).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // viz content
            Constraint::Length(1), // status bar
        ])
        .split(area);

    let content_area = chunks[0];
    let status_area = chunks[1];

    // Update viewport dimensions from terminal size.
    app.scroll.viewport_height = content_area.height as usize;
    app.scroll.viewport_width = content_area.width as usize;

    // Viz content
    draw_viz_content(frame, app, content_area);

    // Vertical scrollbar (only if content overflows)
    if app.scroll.content_height > app.scroll.viewport_height {
        draw_scrollbar(frame, app, content_area);
    }

    // Status bar
    draw_status_bar(frame, app, status_area);

    // Help overlay on top of everything
    if app.show_help {
        draw_help_overlay(frame);
    }
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

    // Build lines for the visible range.
    // Each visible row maps to an original line index via visible_to_original.
    let mut text_lines: Vec<Line> = Vec::with_capacity(end - start);

    for visible_idx in start..end {
        let orig_idx = app.visible_to_original(visible_idx);

        // Get the ANSI line and parse it.
        let ansi_line = app.lines.get(orig_idx).map(|s| s.as_str()).unwrap_or("");
        let base_line: Line = match ansi_to_tui::IntoText::into_text(&ansi_line) {
            Ok(text) => text.lines.into_iter().next().unwrap_or_default(),
            Err(_) => {
                let plain = app.plain_lines.get(orig_idx).map(|s| s.as_str()).unwrap_or("");
                Line::from(plain)
            }
        };

        if has_search {
            if let Some(fuzzy_match) = app.match_for_line(orig_idx) {
                // This line has a fuzzy match — highlight matched characters.
                let is_current = current_match_orig_line == Some(orig_idx);
                let mut highlighted = highlight_fuzzy_match(base_line, &fuzzy_match.char_positions, is_current);
                if is_current {
                    highlighted = highlighted.style(Style::default().bg(Color::Yellow));
                }
                text_lines.push(highlighted);
            } else {
                // Non-matching line in filtered view: show dimmed.
                let dimmed = base_line.style(Style::default().fg(Color::DarkGray));
                text_lines.push(dimmed);
            }
        } else if jump_target_line == Some(orig_idx) {
            // Transient highlight on the line we jumped to after Enter.
            text_lines.push(base_line.style(Style::default().bg(Color::Yellow)));
        } else {
            text_lines.push(base_line);
        }
    }

    let text = Text::from(text_lines);

    // Apply horizontal scroll.
    let paragraph = Paragraph::new(text).scroll((0, app.scroll.offset_x as u16));

    frame.render_widget(paragraph, area);
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
        if !current_buf.is_empty() && (is_match != current_is_match || *base_style != current_base_style) {
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

fn draw_scrollbar(frame: &mut Frame, app: &VizApp, area: Rect) {
    let mut state = ScrollbarState::new(app.scroll.content_height)
        .position(app.scroll.offset_y);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
    frame.render_stateful_widget(scrollbar, area, &mut state);
}

fn draw_status_bar(frame: &mut Frame, app: &VizApp, area: Rect) {
    if app.search_active {
        // Search input mode: show the search prompt with cursor.
        let mut spans = vec![
            Span::styled(
                format!(" /{}", app.search_input),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled("_", Style::default().fg(Color::Yellow).add_modifier(Modifier::SLOW_BLINK)),
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

        let bar = Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::DarkGray));
        frame.render_widget(bar, area);
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
                "  [Tab: next  Shift-Tab: prev  /: new search  Esc: clear]",
                Style::default().fg(Color::Rgb(100, 100, 100)),
            ),
        ];

        // Scroll position
        spans.push(Span::styled("  ", Style::default()));
        spans.push(Span::styled(
            format!("L{}/{}", app.scroll.offset_y + 1, app.scroll.content_height),
            Style::default().fg(Color::DarkGray),
        ));

        let bar = Paragraph::new(Line::from(spans))
            .style(Style::default().bg(Color::DarkGray));
        frame.render_widget(bar, area);
        return;
    }

    let c = &app.task_counts;
    let mut spans = vec![
        Span::styled(
            format!(
                " {} tasks ({} done, {} open, {} active",
                c.total, c.done, c.open, c.in_progress
            ),
            Style::default().fg(Color::White),
        ),
    ];

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

    // Scroll position
    spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(
        format!(
            "L{}/{} ",
            app.scroll.offset_y + 1,
            app.scroll.content_height
        ),
        Style::default().fg(Color::White),
    ));

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

    // Help hint
    spans.push(Span::styled("| ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled("?:help ", Style::default().fg(Color::DarkGray)));

    let bar = Paragraph::new(Line::from(spans))
        .style(Style::default().bg(Color::DarkGray));
    frame.render_widget(bar, area);
}

fn draw_help_overlay(frame: &mut Frame) {
    let size = frame.area();
    let width = 56.min(size.width.saturating_sub(4));
    let height = 27.min(size.height.saturating_sub(4));
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
            Span::styled(
                format!("  {:<14}", key),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(desc.to_string(), Style::default().fg(Color::White)),
        ])
    };

    let blank = || Line::from("");

    let lines = vec![
        heading("Navigation"),
        binding("j / k", "Scroll down / up"),
        binding("h / l", "Scroll left / right"),
        binding("Ctrl-d / u", "Page down / up"),
        binding("g / G", "Jump to top / bottom"),
        blank(),
        heading("Search (vim-style)"),
        binding("/", "Start search"),
        binding("Enter", "Accept (show all, keep highlights)"),
        binding("Esc", "Clear search"),
        binding("n / N / Tab", "Next / previous match"),
        blank(),
        heading("While searching"),
        binding("Tab / ←→", "Next / previous match"),
        binding("Up / Down", "Scroll view"),
        binding("Ctrl-u", "Clear search input"),
        blank(),
        heading("General"),
        binding("t", "Toggle view/total tokens"),
        binding("r", "Force refresh"),
        binding("?", "Toggle this help"),
        binding("q", "Quit"),
        binding("Ctrl-c", "Force quit"),
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

/// Render token breakdown spans: "→in ←out [◎cache] (label) [$cost]"
fn render_token_breakdown<'a>(spans: &mut Vec<Span<'a>>, usage: &TokenUsage, label: &str) {
    let input = format_tokens(usage.total_input());
    let output = format_tokens(usage.output_tokens);

    let cache_total = usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
    let token_str = if cache_total > 0 {
        let cache = format_tokens(cache_total);
        format!("→{} ←{} ◎{}", input, output, cache)
    } else {
        format!("→{} ←{}", input, output)
    };

    spans.push(Span::styled(
        token_str,
        Style::default().fg(Color::Cyan),
    ));

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
