//! Markdown-to-ratatui renderer.
//!
//! Converts markdown text into styled `Vec<Line<'static>>` suitable for
//! rendering in ratatui `Paragraph` widgets. Uses pulldown-cmark for parsing
//! and syntect for fenced code block highlighting.

use std::sync::OnceLock;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::UnicodeWidthStr;

const COLOR_H1: Color = Color::Indexed(75);
const COLOR_H2: Color = Color::Indexed(114);
const COLOR_H3: Color = Color::Indexed(180);
const COLOR_H4: Color = Color::Indexed(174);
const COLOR_H5: Color = Color::Indexed(139);
const COLOR_H6: Color = Color::Indexed(109);
const COLOR_INLINE_CODE_FG: Color = Color::Indexed(222);
const COLOR_INLINE_CODE_BG: Color = Color::Indexed(236);
const COLOR_CODE_BLOCK_BG: Color = Color::Indexed(235);
const COLOR_CODE_BLOCK_BAR: Color = Color::Indexed(240);
const COLOR_LINK: Color = Color::Indexed(75);
const COLOR_LINK_URL: Color = Color::Indexed(245);
const COLOR_BLOCKQUOTE_BAR: Color = Color::Indexed(244);
const COLOR_BLOCKQUOTE_TEXT: Color = Color::Indexed(250);
const COLOR_RULE: Color = Color::Indexed(240);
const COLOR_TABLE_BORDER: Color = Color::Indexed(240);
const COLOR_TABLE_HEADER: Color = Color::Indexed(75);
const BULLETS: &[char] = &['•', '◦', '▸'];

struct SyntectAssets {
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
}

fn syntect_assets() -> &'static SyntectAssets {
    static ASSETS: OnceLock<SyntectAssets> = OnceLock::new();
    ASSETS.get_or_init(|| SyntectAssets {
        syntax_set: SyntaxSet::load_defaults_newlines(),
        theme_set: ThemeSet::load_defaults(),
    })
}

/// Convert a markdown string into styled ratatui [`Line`]s.
///
/// `width` is the available character width for horizontal rules.
pub fn markdown_to_lines(md: &str, width: usize) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    let parser = Parser::new_ext(md, opts);
    let mut renderer = MdRenderer::new(width);
    for event in parser {
        renderer.handle_event(event);
    }
    renderer.finish()
}

struct MdRenderer {
    lines: Vec<Line<'static>>,
    current_spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    list_stack: Vec<Option<u64>>,
    in_code_block: bool,
    code_lang: Option<String>,
    code_buffer: String,
    width: usize,
    blockquote_depth: usize,
    heading_level: Option<HeadingLevel>,
    link_url: Option<String>,
    // Table state
    in_table: bool,
    table_alignments: Vec<Alignment>,
    table_rows: Vec<Vec<String>>, // rows of cells (plain text per cell)
    current_row: Vec<String>,     // cells accumulated for the current row
    current_cell: String,         // text accumulated for the current cell
    table_header_row: bool,       // true when inside TableHead
}

impl MdRenderer {
    fn new(width: usize) -> Self {
        Self {
            lines: Vec::new(),
            current_spans: Vec::new(),
            style_stack: vec![Style::default()],
            list_stack: Vec::new(),
            in_code_block: false,
            code_lang: None,
            code_buffer: String::new(),
            width,
            blockquote_depth: 0,
            heading_level: None,
            link_url: None,
            in_table: false,
            table_alignments: Vec::new(),
            table_rows: Vec::new(),
            current_row: Vec::new(),
            current_cell: String::new(),
            table_header_row: false,
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, modifier: impl FnOnce(Style) -> Style) {
        let new = modifier(self.current_style());
        self.style_stack.push(new);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn flush_line(&mut self) {
        let spans = std::mem::take(&mut self.current_spans);
        if self.blockquote_depth > 0 {
            let mut prefixed = Vec::with_capacity(spans.len() + 1);
            let bar = "▎ ".repeat(self.blockquote_depth);
            prefixed.push(Span::styled(bar, Style::default().fg(COLOR_BLOCKQUOTE_BAR)));
            prefixed.extend(spans);
            self.lines.push(Line::from(prefixed));
        } else {
            self.lines.push(Line::from(spans));
        }
    }

    fn blank_line(&mut self) {
        self.flush_line();
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                if !self.lines.is_empty() {
                    self.blank_line();
                }
                self.heading_level = Some(level);
                let color = heading_color(level);
                self.push_style(|s| s.fg(color).add_modifier(Modifier::BOLD));
            }
            Event::Start(Tag::Paragraph) => {
                // Only insert a blank separator when current_spans is empty.
                // Inside a list item, current_spans holds the marker (e.g. "1. ");
                // flushing here would strand the marker on its own line, separated
                // from the item text.
                if !self.lines.is_empty() && !self.in_code_block && self.current_spans.is_empty() {
                    self.blank_line();
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                self.blockquote_depth += 1;
                self.push_style(|s| s.fg(COLOR_BLOCKQUOTE_TEXT));
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                self.in_code_block = true;
                self.code_buffer.clear();
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => {
                        let l = lang.to_string();
                        if l.is_empty() { None } else { Some(l) }
                    }
                    CodeBlockKind::Indented => None,
                };
                if !self.lines.is_empty() {
                    self.blank_line();
                }
            }
            Event::Start(Tag::List(start)) => {
                if self.list_stack.is_empty() {
                    if !self.lines.is_empty() {
                        self.blank_line();
                    }
                } else if !self.current_spans.is_empty() {
                    self.flush_line();
                }
                self.list_stack.push(start);
            }
            Event::Start(Tag::Item) => {
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last() {
                    Some(Some(n)) => {
                        let marker = format!("{indent}{}. ", n);
                        if let Some(Some(counter)) = self.list_stack.last_mut() {
                            *counter += 1;
                        }
                        marker
                    }
                    _ => {
                        let bullet = BULLETS[depth % BULLETS.len()];
                        format!("{indent}{bullet} ")
                    }
                };
                self.current_spans
                    .push(Span::styled(marker, Style::default()));
            }
            Event::Start(Tag::Emphasis) => {
                self.push_style(|s| s.add_modifier(Modifier::ITALIC));
            }
            Event::Start(Tag::Strong) => {
                self.push_style(|s| s.add_modifier(Modifier::BOLD));
            }
            Event::Start(Tag::Strikethrough) => {
                self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT));
            }
            Event::Start(Tag::Table(alignments)) => {
                if !self.lines.is_empty() {
                    self.blank_line();
                }
                self.in_table = true;
                self.table_alignments = alignments;
                self.table_rows.clear();
            }
            Event::Start(Tag::TableHead) => {
                self.table_header_row = true;
                self.current_row.clear();
            }
            Event::Start(Tag::TableRow) => {
                self.table_header_row = false;
                self.current_row.clear();
            }
            Event::Start(Tag::TableCell) => {
                self.current_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                self.current_row
                    .push(std::mem::take(&mut self.current_cell));
            }
            Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                self.table_rows.push(std::mem::take(&mut self.current_row));
            }
            Event::End(TagEnd::Table) => {
                self.emit_table();
                self.in_table = false;
                self.table_alignments.clear();
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                self.link_url = Some(dest_url.to_string());
                self.push_style(|s| s.fg(COLOR_LINK).add_modifier(Modifier::UNDERLINED));
            }
            Event::End(TagEnd::Heading(_level)) => {
                self.flush_line();
                self.pop_style();
                // Don't add blank line here — the following Start(Paragraph)
                // already inserts one, so adding one here would double-space.
                self.heading_level = None;
            }
            Event::End(TagEnd::Paragraph) => {
                self.flush_line();
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.flush_line();
                self.pop_style();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
            }
            Event::End(TagEnd::CodeBlock) => {
                self.emit_code_block();
                self.in_code_block = false;
                self.code_lang = None;
            }
            Event::End(TagEnd::List(_)) => {
                self.list_stack.pop();
            }
            Event::End(TagEnd::Item) => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
            }
            Event::End(TagEnd::Emphasis) => {
                self.pop_style();
            }
            Event::End(TagEnd::Strong) => {
                self.pop_style();
            }
            Event::End(TagEnd::Strikethrough) => {
                self.pop_style();
            }
            Event::End(TagEnd::Link) => {
                self.pop_style();
                if let Some(url) = self.link_url.take()
                    && !url.is_empty()
                {
                    self.current_spans.push(Span::styled(
                        format!(" ({url})"),
                        Style::default().fg(COLOR_LINK_URL),
                    ));
                }
            }
            Event::Text(text) => {
                if self.in_code_block {
                    self.code_buffer.push_str(&text);
                } else if self.in_table {
                    self.current_cell.push_str(&text);
                } else {
                    let style = self.current_style();
                    self.current_spans
                        .push(Span::styled(text.to_string(), style));
                }
            }
            Event::Code(code) => {
                if self.in_table {
                    self.current_cell.push_str(&code);
                } else {
                    let style = Style::default()
                        .fg(COLOR_INLINE_CODE_FG)
                        .bg(COLOR_INLINE_CODE_BG);
                    self.current_spans
                        .push(Span::styled(format!(" {code} "), style));
                }
            }
            Event::SoftBreak => {
                if self.in_code_block {
                    self.code_buffer.push('\n');
                } else if self.in_table {
                    self.current_cell.push(' ');
                } else {
                    // In a TUI, soft breaks should produce actual line breaks
                    // rather than spaces. This preserves newlines in tool output
                    // and other pre-formatted content that isn't in code blocks.
                    self.flush_line();
                }
            }
            Event::HardBreak => {
                self.flush_line();
            }
            Event::Rule => {
                if !self.lines.is_empty() {
                    self.blank_line();
                }
                let rule_width = self.width.min(120);
                let rule = "━".repeat(rule_width);
                self.lines.push(Line::from(Span::styled(
                    rule,
                    Style::default().fg(COLOR_RULE),
                )));
            }
            _ => {}
        }
    }

    fn emit_code_block(&mut self) {
        let assets = syntect_assets();
        let theme = &assets.theme_set.themes["base16-ocean.dark"];
        let bar_span = Span::styled("│ ", Style::default().fg(COLOR_CODE_BLOCK_BAR));
        let bg_style = Style::default().bg(COLOR_CODE_BLOCK_BG);
        let syntax = self
            .code_lang
            .as_deref()
            .and_then(|lang| assets.syntax_set.find_syntax_by_token(lang))
            .unwrap_or_else(|| assets.syntax_set.find_syntax_plain_text());
        let mut highlighter = HighlightLines::new(syntax, theme);
        for line_text in LinesWithEndings::from(&self.code_buffer) {
            let mut spans = vec![bar_span.clone()];
            match highlighter.highlight_line(line_text, &assets.syntax_set) {
                Ok(ranges) => {
                    for (style, text) in ranges {
                        let trimmed = text.trim_end_matches('\n');
                        if trimmed.is_empty() && text.contains('\n') {
                            continue;
                        }
                        let span = crate::tui::syntect_convert::into_span((style, trimmed));
                        let mut s = span.style;
                        s = s.bg(COLOR_CODE_BLOCK_BG);
                        spans.push(Span::styled(span.content.into_owned(), s));
                    }
                }
                Err(_) => {
                    let trimmed = line_text.trim_end_matches('\n');
                    spans.push(Span::styled(trimmed.to_owned(), bg_style));
                }
            }
            self.lines.push(Line::from(spans));
        }
    }

    fn emit_table(&mut self) {
        if self.table_rows.is_empty() {
            return;
        }

        let border_style = Style::default().fg(COLOR_TABLE_BORDER);
        let header_style = Style::default()
            .fg(COLOR_TABLE_HEADER)
            .add_modifier(Modifier::BOLD);

        // Compute the number of columns and max width per column.
        let num_cols = self
            .table_rows
            .iter()
            .map(|row| row.len())
            .max()
            .unwrap_or(0);
        if num_cols == 0 {
            return;
        }

        let mut col_widths = vec![0usize; num_cols];
        for row in &self.table_rows {
            for (c, cell) in row.iter().enumerate() {
                col_widths[c] = col_widths[c].max(cell.width());
            }
        }
        // Ensure minimum column width of 3.
        for w in &mut col_widths {
            *w = (*w).max(3);
        }

        // Helper: build a horizontal border line.
        let make_border = |left: &str, mid: &str, right: &str| -> Line<'static> {
            let mut s = String::from(left);
            for (i, &w) in col_widths.iter().enumerate() {
                s.push_str(&"─".repeat(w + 2)); // +2 for padding
                if i < num_cols - 1 {
                    s.push_str(mid);
                }
            }
            s.push_str(right);
            Line::from(Span::styled(s, border_style))
        };

        // Helper: build a data row.
        let make_row = |cells: &[String], style: Style| -> Line<'static> {
            let mut spans = Vec::new();
            spans.push(Span::styled("│", border_style));
            for (c, w) in col_widths.iter().enumerate() {
                let text = cells.get(c).map(|s| s.as_str()).unwrap_or("");
                let text_w = text.width();
                let padding = w.saturating_sub(text_w);
                let align = self
                    .table_alignments
                    .get(c)
                    .copied()
                    .unwrap_or(Alignment::None);
                let (pad_left, pad_right) = match align {
                    Alignment::Center => (padding / 2, padding - padding / 2),
                    Alignment::Right => (padding, 0),
                    _ => (0, padding),
                };
                spans.push(Span::styled(
                    format!(
                        " {}{}{} ",
                        " ".repeat(pad_left),
                        text,
                        " ".repeat(pad_right)
                    ),
                    style,
                ));
                spans.push(Span::styled("│", border_style));
            }
            Line::from(spans)
        };

        // Top border.
        self.lines.push(make_border("┌", "┬", "┐"));

        // Header row (first row).
        if let Some(header) = self.table_rows.first() {
            self.lines.push(make_row(header, header_style));
            // Header separator.
            self.lines.push(make_border("├", "┼", "┤"));
        }

        // Body rows.
        for row in self.table_rows.iter().skip(1) {
            self.lines.push(make_row(row, Style::default()));
        }

        // Bottom border.
        self.lines.push(make_border("└", "┴", "┘"));
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current_spans.is_empty() {
            self.flush_line();
        }
        self.lines
    }
}

fn heading_color(level: HeadingLevel) -> Color {
    match level {
        HeadingLevel::H1 => COLOR_H1,
        HeadingLevel::H2 => COLOR_H2,
        HeadingLevel::H3 => COLOR_H3,
        HeadingLevel::H4 => COLOR_H4,
        HeadingLevel::H5 => COLOR_H5,
        HeadingLevel::H6 => COLOR_H6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn has_modifier(line: &Line, modifier: Modifier) -> bool {
        line.spans
            .iter()
            .any(|s| s.style.add_modifier.contains(modifier))
    }

    fn has_fg_color(line: &Line, color: Color) -> bool {
        line.spans.iter().any(|s| s.style.fg == Some(color))
    }

    #[test]
    fn test_bold_text() {
        let lines = markdown_to_lines("**bold**", 80);
        assert_eq!(lines.len(), 1);
        assert!(has_modifier(&lines[0], Modifier::BOLD));
    }

    #[test]
    fn test_italic_text() {
        let lines = markdown_to_lines("*italic*", 80);
        assert_eq!(lines.len(), 1);
        assert!(has_modifier(&lines[0], Modifier::ITALIC));
    }

    #[test]
    fn test_heading_levels() {
        for (md, color) in [
            ("# H1", COLOR_H1),
            ("## H2", COLOR_H2),
            ("### H3", COLOR_H3),
            ("#### H4", COLOR_H4),
            ("##### H5", COLOR_H5),
            ("###### H6", COLOR_H6),
        ] {
            let lines = markdown_to_lines(md, 80);
            assert!(!lines.is_empty());
            assert!(has_fg_color(&lines[0], color));
            assert!(has_modifier(&lines[0], Modifier::BOLD));
        }
    }

    #[test]
    fn test_unordered_list() {
        let lines = markdown_to_lines("- first\n- second", 80);
        assert_eq!(lines.len(), 2);
        assert!(line_text(&lines[0]).contains("• first"));
    }

    #[test]
    fn test_empty_input() {
        let lines = markdown_to_lines("", 80);
        assert!(lines.is_empty());
    }

    #[test]
    fn test_plain_text() {
        let lines = markdown_to_lines("just text", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "just text");
    }

    #[test]
    fn test_code_block() {
        let lines = markdown_to_lines("```rust\nfn main() {}\n```", 80);
        assert!(!lines.is_empty());
        let text = line_text(&lines[lines.len() - 1]);
        assert!(text.contains('│'));
    }

    #[test]
    fn test_table_rendering() {
        let md = "| Name | Age |\n|------|-----|\n| Alice | 30 |\n| Bob | 25 |";
        let lines = markdown_to_lines(md, 80);
        assert!(!lines.is_empty(), "table should produce lines");

        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();

        // Should have box-drawing borders.
        assert!(texts[0].contains('┌'), "first line should be top border");
        assert!(
            texts.last().unwrap().contains('└'),
            "last line should be bottom border"
        );

        // Should contain cell content.
        let all_text = texts.join("\n");
        assert!(all_text.contains("Alice"), "table should contain Alice");
        assert!(all_text.contains("Bob"), "table should contain Bob");
        assert!(all_text.contains("Age"), "table should contain Age header");

        // Should have header separator.
        assert!(
            texts.iter().any(|t| t.contains('┼')),
            "should have header separator with ┼"
        );
    }

    #[test]
    fn test_table_alignment() {
        let md = "| Left | Center | Right |\n|:-----|:------:|------:|\n| a | b | c |";
        let lines = markdown_to_lines(md, 80);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        let all_text = texts.join("\n");
        assert!(all_text.contains("Left"));
        assert!(all_text.contains("Center"));
        assert!(all_text.contains("Right"));
    }

    #[test]
    fn test_soft_breaks_produce_newlines() {
        // Simulates tool output format: consecutive lines should stay separate.
        let md = "┌─ Bash ────\n│ $ ls\n│ file.txt\n└─";
        let lines = markdown_to_lines(md, 80);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        assert!(
            texts.len() >= 4,
            "each line should be separate, got {} lines: {:?}",
            texts.len(),
            texts
        );
        assert!(texts[0].contains("┌─ Bash"), "first line: tool header");
        assert!(texts[1].contains("│ $ ls"), "second line: command");
        assert!(texts[2].contains("│ file.txt"), "third line: output");
        assert!(texts[3].contains("└─"), "fourth line: closing box");
    }

    #[test]
    fn test_wgnex_chat_transcript_format_renders() {
        // Exact format the wg-nex agent loop now writes to
        // `chat/<ref>/.streaming` on a multi-step tool turn. This is
        // the production output captured from a live qwen3-coder-30b
        // run against `Run bash: echo first && echo second. Report
        // what you see.`
        //
        // Regressed in commit 94e74333 (which mirrored stderr-style
        // `> name(args)` markers that markdown parsed as blockquotes).
        // Fixed in 432b8da9 (switched to the box-drawing format).
        // This test locks that in.
        let md = "\n┌─ bash ────────────────────────────────\n\
                  │ $ echo first && echo second\n\
                  │ first\n\
                  │ second\n\
                  └─\n\
                  I ran the bash command `echo first && echo second` and here are the results:\n\n\
                  first\nsecond\n\n\
                  The command executed successfully.";
        let lines = markdown_to_lines(md, 80);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        let all_text = texts.join("\n");

        // Box drawing glyphs survive and land on their own lines —
        // the TUI's tool-box renderer looks for `starts_with("┌─")`
        // at line start, so this is load-bearing.
        assert!(
            texts.iter().any(|t| t.trim_start().starts_with("┌─ bash")),
            "expected `┌─ bash` header as an independent line, got {:?}",
            texts
        );
        assert!(
            texts
                .iter()
                .any(|t| t.trim_start().starts_with("│ $ echo first")),
            "expected `│ $ echo first` command line, got {:?}",
            texts
        );
        assert!(
            texts.iter().any(|t| t.trim_start().starts_with("│ first")),
            "expected `│ first` output line, got {:?}",
            texts
        );
        assert!(
            texts.iter().any(|t| t.trim_start().starts_with("└─")),
            "expected `└─` closing line, got {:?}",
            texts
        );

        // Model prose after `└─` renders as normal markdown — not as
        // blockquote, not as code fence, not as part of the box.
        assert!(
            all_text.contains("I ran the bash command"),
            "model prose after the box should be preserved, got {}",
            all_text
        );

        // Critical negative assertion: the old regressed format used
        // `> name(args)` which markdown parses as a blockquote. None
        // of our output should begin with a naked `>` at col 0 after
        // trim — if it does, we're back in the bad state.
        for t in &texts {
            let trimmed = t.trim_start();
            assert!(
                !trimmed.starts_with("> bash("),
                "found stderr-style marker in rendered output (would render as blockquote): {}",
                t
            );
        }
    }

    #[test]
    fn test_wgnex_chat_transcript_error_format_renders() {
        // Error case: tool output prefixed with `×` inside the box,
        // closed with `└─`. Verifies the error path also renders
        // as a proper tool box rather than a blockquote or bad glyphs.
        let md = "\n┌─ bash ────────────────────────────────\n\
                  │ $ exit 1\n\
                  │ × bash: exit 1 returned non-zero\n\
                  │ ... (3 more lines)\n\
                  └─\n\
                  The command failed.";
        let lines = markdown_to_lines(md, 80);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();

        assert!(texts.iter().any(|t| t.trim_start().starts_with("┌─ bash")));
        assert!(
            texts.iter().any(|t| t.trim_start().starts_with("│ × bash")),
            "error content inside box, got {:?}",
            texts
        );
        assert!(
            texts.iter().any(|t| t.contains("... (3 more lines)")),
            "truncation line inside box, got {:?}",
            texts
        );
        assert!(texts.iter().any(|t| t.trim_start().starts_with("└─")));
        assert!(
            texts.iter().any(|t| t.contains("The command failed.")),
            "model prose after box, got {:?}",
            texts
        );
    }

    #[test]
    fn test_ordered_list_compact() {
        // Numbered list items must not have blank lines between number and content.
        let md = "1. First\n2. Second\n3. Third";
        let lines = markdown_to_lines(md, 80);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        assert_eq!(texts.len(), 3, "3 items = 3 lines, got {:?}", texts);
        assert!(texts[0].contains("1.") && texts[0].contains("First"));
        assert!(texts[1].contains("2.") && texts[1].contains("Second"));
        assert!(texts[2].contains("3.") && texts[2].contains("Third"));
    }

    #[test]
    fn test_ordered_list_loose_compact() {
        // Even "loose" lists (blank lines between items in source) should
        // render each item on a single line with marker + content together.
        let md = "1. First\n\n2. Second\n\n3. Third";
        let lines = markdown_to_lines(md, 80);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        // Each item should have marker and content on the same line.
        let item_lines: Vec<&String> = texts.iter().filter(|t| !t.is_empty()).collect();
        assert_eq!(
            item_lines.len(),
            3,
            "3 non-blank lines for 3 items, got {:?}",
            texts
        );
        assert!(item_lines[0].contains("1.") && item_lines[0].contains("First"));
        assert!(item_lines[1].contains("2.") && item_lines[1].contains("Second"));
        assert!(item_lines[2].contains("3.") && item_lines[2].contains("Third"));
    }

    #[test]
    fn test_bullet_list_compact() {
        let md = "- Alpha\n\n- Beta\n\n- Gamma";
        let lines = markdown_to_lines(md, 80);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        let item_lines: Vec<&String> = texts.iter().filter(|t| !t.is_empty()).collect();
        assert_eq!(
            item_lines.len(),
            3,
            "3 non-blank lines for 3 items, got {:?}",
            texts
        );
        assert!(item_lines[0].contains("Alpha"));
        assert!(item_lines[1].contains("Beta"));
        assert!(item_lines[2].contains("Gamma"));
    }

    #[test]
    fn test_nested_list() {
        let md = "1. Outer\n   - Inner A\n   - Inner B\n2. Second";
        let lines = markdown_to_lines(md, 80);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        // Outer items should have their markers with content.
        assert!(
            texts
                .iter()
                .any(|t| t.contains("1.") && t.contains("Outer")),
            "outer item 1 should be compact: {:?}",
            texts
        );
        assert!(
            texts.iter().any(|t| t.contains("Inner A")),
            "nested item A present: {:?}",
            texts
        );
        assert!(
            texts.iter().any(|t| t.contains("Inner B")),
            "nested item B present: {:?}",
            texts
        );
    }

    #[test]
    fn test_heading_no_double_blank_line() {
        let md = "## Title\n\nContent here";
        let lines = markdown_to_lines(md, 80);
        let texts: Vec<String> = lines.iter().map(|l| line_text(l)).collect();
        // Should be: heading, blank, content (3 lines).
        // NOT: heading, blank, blank, content (4 lines).
        assert_eq!(
            texts.len(),
            3,
            "heading + blank + content = 3 lines, got {:?}",
            texts
        );
        assert!(texts[0].contains("Title"));
        assert_eq!(texts[1], ""); // single blank line
        assert!(texts[2].contains("Content"));
    }
}
