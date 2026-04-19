use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;
use tui_tree_widget::{TreeItem, TreeState};

/// Which pane has focus within the file browser.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FileBrowserFocus {
    Tree,
    Preview,
}

/// Cached preview of a file's content.
pub struct PreviewCache {
    pub path: PathBuf,
    pub lines: Vec<Line<'static>>,
    pub line_count: usize,
    pub file_size: u64,
    pub truncated: bool,
    pub is_binary: bool,
}

/// Maximum lines to load from a file for preview.
const MAX_PREVIEW_LINES: usize = 1000;

/// Lazily-initialized syntect assets (syntax set + theme).
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

/// Map a file extension to a syntect syntax name.
/// Returns None for plain text (no highlighting).
fn syntax_for_extension<'a>(assets: &'a SyntectAssets, ext: &str) -> Option<&'a SyntaxReference> {
    let lookup = match ext {
        "toml" => "TOML",
        "yaml" | "yml" => "YAML",
        "json" | "jsonl" => "JSON",
        "md" | "markdown" => "Markdown",
        "sh" | "bash" | "zsh" => "Bourne Again Shell (bash)",
        "rs" => "Rust",
        _ => return None,
    };
    assets.syntax_set.find_syntax_by_name(lookup)
}

/// State for the file browser tab.
pub struct FileBrowser {
    /// Root directory being browsed (.workgraph path).
    pub root: PathBuf,
    /// Tree state for tui-tree-widget (selection, expansion, scroll).
    pub tree_state: TreeState<String>,
    /// Built tree items (rebuilt on refresh).
    pub tree_items: Vec<TreeItem<'static, String>>,
    /// Cached preview content for the currently selected file.
    pub preview_cache: Option<PreviewCache>,
    /// Focus within the file browser (Tree or Preview).
    pub focus: FileBrowserFocus,
    /// Preview scroll offset (vertical line offset).
    pub preview_scroll: usize,
    /// Whether search mode is active (input field shown).
    pub searching: bool,
    /// Current search query text.
    pub search_query: String,
}

impl FileBrowser {
    /// Create a new file browser rooted at the given .workgraph directory.
    pub fn new(workgraph_dir: &Path) -> Self {
        let tree_items = build_tree(workgraph_dir);
        let mut tree_state = TreeState::default();
        // Select the first item if any exist
        if !tree_items.is_empty() {
            tree_state.select_first();
        }
        Self {
            root: workgraph_dir.to_path_buf(),
            tree_state,
            tree_items,
            preview_cache: None,
            focus: FileBrowserFocus::Tree,
            preview_scroll: 0,
            searching: false,
            search_query: String::new(),
        }
    }

    /// Rebuild the tree from the filesystem (respects active search filter).
    pub fn refresh(&mut self) {
        if self.searching && !self.search_query.is_empty() {
            self.rebuild_filtered();
        } else {
            self.tree_items = build_tree(&self.root);
        }
    }

    /// Enter search mode.
    pub fn enter_search(&mut self) {
        self.searching = true;
        self.search_query.clear();
    }

    /// Exit search mode and restore the full tree.
    pub fn exit_search(&mut self) {
        self.searching = false;
        self.search_query.clear();
        self.tree_items = build_tree(&self.root);
        if !self.tree_items.is_empty() {
            self.tree_state.select_first();
        }
    }

    /// Push a character onto the search query and rebuild the filtered tree.
    pub fn search_push(&mut self, ch: char) {
        self.search_query.push(ch);
        self.rebuild_filtered();
    }

    /// Pop a character from the search query and rebuild the filtered tree.
    pub fn search_pop(&mut self) {
        self.search_query.pop();
        if self.search_query.is_empty() {
            self.tree_items = build_tree(&self.root);
        } else {
            self.rebuild_filtered();
        }
        if !self.tree_items.is_empty() {
            self.tree_state.select_first();
        }
    }

    /// Rebuild the tree with fuzzy filtering applied.
    fn rebuild_filtered(&mut self) {
        let matcher = SkimMatcherV2::default();
        let (items, open_ids) = build_tree_filtered(&self.root, &self.search_query, &matcher);
        self.tree_items = items;
        // Auto-expand directories containing matches
        for id in &open_ids {
            self.tree_state.open(id.clone());
        }
        if !self.tree_items.is_empty() {
            self.tree_state.select_first();
        }
    }

    /// Get the currently selected file path (if a file is selected).
    pub fn selected_path(&self) -> Option<PathBuf> {
        let selected = self.tree_state.selected();
        if selected.is_empty() {
            return None;
        }
        let mut path = self.root.clone();
        for segment in selected {
            path.push(segment);
        }
        Some(path)
    }

    /// Load preview for the currently selected path.
    /// Returns true if the preview changed.
    pub fn load_preview(&mut self) -> bool {
        let path = match self.selected_path() {
            Some(p) if p.is_file() => p,
            _ => {
                let changed = self.preview_cache.is_some();
                self.preview_cache = None;
                return changed;
            }
        };

        // Skip reload if already cached for this path
        if let Some(ref cache) = self.preview_cache
            && cache.path == path
        {
            return false;
        }

        self.preview_scroll = 0;
        self.preview_cache = Some(load_file_preview(&path));
        true
    }

    /// Scroll the preview pane up.
    pub fn preview_scroll_up(&mut self, amount: usize) {
        self.preview_scroll = self.preview_scroll.saturating_sub(amount);
    }

    /// Scroll the preview pane down.
    pub fn preview_scroll_down(&mut self, amount: usize) {
        if let Some(ref cache) = self.preview_cache {
            let max = cache.lines.len().saturating_sub(1);
            self.preview_scroll = (self.preview_scroll + amount).min(max);
        }
    }

    /// Jump preview to top.
    pub fn preview_go_top(&mut self) {
        self.preview_scroll = 0;
    }

    /// Jump preview to bottom.
    pub fn preview_go_bottom(&mut self) {
        if let Some(ref cache) = self.preview_cache {
            self.preview_scroll = cache.lines.len().saturating_sub(1);
        }
    }
}

/// Build the tree of TreeItems from the filesystem.
fn build_tree(root: &Path) -> Vec<TreeItem<'static, String>> {
    build_children(root)
}

/// Recursively build children for a directory.
fn build_children(dir: &Path) -> Vec<TreeItem<'static, String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    let mut files: Vec<(String, PathBuf)> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();

        // Skip .git directories and lock files
        if name == ".git" || name.ends_with(".lock") {
            continue;
        }

        if path.is_dir() {
            dirs.push((name, path));
        } else {
            files.push((name, path));
        }
    }

    // Sort: directories first (alphabetical), then files (alphabetical)
    dirs.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

    let mut items = Vec::new();

    for (name, path) in dirs {
        let children = build_children(&path);
        let item = TreeItem::new(name.clone(), format!("{}/", name), children)
            .unwrap_or_else(|_| TreeItem::new_leaf(name.clone(), format!("{}/", name)));
        items.push(item);
    }

    for (name, _path) in files {
        items.push(TreeItem::new_leaf(name.clone(), name));
    }

    items
}

/// Build a filtered tree with fuzzy matching.
/// Returns (tree_items, identifiers_to_auto_open).
fn build_tree_filtered(
    root: &Path,
    query: &str,
    matcher: &SkimMatcherV2,
) -> (Vec<TreeItem<'static, String>>, Vec<Vec<String>>) {
    let mut open_ids = Vec::new();
    let items = build_children_filtered(root, query, matcher, &[], &mut open_ids);
    (items, open_ids)
}

/// Recursively build children with fuzzy filtering.
/// Returns only items whose name matches or that have matching descendants.
fn build_children_filtered(
    dir: &Path,
    query: &str,
    matcher: &SkimMatcherV2,
    parent_id: &[String],
    open_ids: &mut Vec<Vec<String>>,
) -> Vec<TreeItem<'static, String>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut dirs: Vec<(String, PathBuf)> = Vec::new();
    let mut files: Vec<(String, PathBuf)> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();

        if name == ".git" || name.ends_with(".lock") {
            continue;
        }

        if path.is_dir() {
            dirs.push((name, path));
        } else {
            files.push((name, path));
        }
    }

    dirs.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    files.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));

    let mut items = Vec::new();

    for (name, path) in dirs {
        let mut this_id = parent_id.to_vec();
        this_id.push(name.clone());

        let children = build_children_filtered(&path, query, matcher, &this_id, open_ids);
        let dir_matches = matcher.fuzzy_indices(&name, query);

        if !children.is_empty() || dir_matches.is_some() {
            let display = match dir_matches {
                Some((_score, indices)) => highlight_name(&name, &indices, "/"),
                None => Line::raw(format!("{}/", name)),
            };
            // Check before moving children into TreeItem
            let has_children = !children.is_empty();
            let item = TreeItem::new(name.clone(), display, children)
                .unwrap_or_else(|_| TreeItem::new_leaf(name.clone(), format!("{}/", name)));
            items.push(item);

            // Auto-expand this directory since it contains matches
            if has_children {
                open_ids.push(this_id);
            }
        }
    }

    for (name, _path) in files {
        if let Some((_score, indices)) = matcher.fuzzy_indices(&name, query) {
            let display = highlight_name(&name, &indices, "");
            items.push(TreeItem::new_leaf(name.clone(), display));
        }
    }

    items
}

/// Create a styled Line highlighting the matched character positions.
fn highlight_name(name: &str, indices: &[usize], suffix: &str) -> Line<'static> {
    let match_set: HashSet<usize> = indices.iter().copied().collect();
    let match_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default();

    let chars: Vec<char> = name.chars().collect();
    let mut spans = Vec::new();
    let mut current = String::new();
    let mut current_is_match = false;

    for (i, &ch) in chars.iter().enumerate() {
        let is_match = match_set.contains(&i);
        if is_match != current_is_match && !current.is_empty() {
            let style = if current_is_match {
                match_style
            } else {
                normal_style
            };
            spans.push(Span::styled(current.clone(), style));
            current.clear();
        }
        current.push(ch);
        current_is_match = is_match;
    }

    // Append suffix (e.g. "/" for directories) to the last chunk
    current.push_str(suffix);
    if !current.is_empty() {
        let style = if current_is_match {
            match_style
        } else {
            normal_style
        };
        spans.push(Span::styled(current, style));
    }

    Line::from(spans)
}

/// Load a file for preview, handling binary files and truncation.
fn load_file_preview(path: &Path) -> PreviewCache {
    let file_size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    // Try to read the file
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            return PreviewCache {
                path: path.to_path_buf(),
                lines: vec![Line::raw(format!("Error reading file: {e}"))],
                line_count: 1,
                file_size,
                truncated: false,
                is_binary: false,
            };
        }
    };

    // Check for binary content (look for null bytes in first 8KB)
    let check_len = bytes.len().min(8192);
    if bytes[..check_len].contains(&0) {
        return PreviewCache {
            path: path.to_path_buf(),
            lines: vec![Line::raw(format!(
                "Binary file, {} bytes",
                format_size(file_size)
            ))],
            line_count: 1,
            file_size,
            truncated: false,
            is_binary: true,
        };
    }

    let content = String::from_utf8_lossy(&bytes).into_owned();
    let total_lines = content.lines().count();
    let truncated = total_lines > MAX_PREVIEW_LINES;

    // Determine line number gutter width
    let display_count = total_lines.min(MAX_PREVIEW_LINES);
    let num_width = format!("{}", display_count).len();

    // Try syntax highlighting
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let assets = syntect_assets();
    let syntax = syntax_for_extension(assets, ext);

    let lines = if let Some(syn) = syntax {
        highlight_lines(&content, syn, assets, num_width, display_count)
    } else {
        plain_lines(&content, num_width, display_count)
    };

    let mut lines = lines;
    if truncated {
        lines.push(Line::raw(format!(
            "[truncated at {} lines, {} total]",
            MAX_PREVIEW_LINES, total_lines
        )));
    }

    PreviewCache {
        path: path.to_path_buf(),
        lines,
        line_count: total_lines,
        file_size,
        truncated,
        is_binary: false,
    }
}

/// Build plain-text lines with line numbers (no highlighting).
fn plain_lines(content: &str, num_width: usize, max_lines: usize) -> Vec<Line<'static>> {
    content
        .lines()
        .take(max_lines)
        .enumerate()
        .map(|(i, line)| Line::raw(format!("{:>width$} │ {}", i + 1, line, width = num_width)))
        .collect()
}

/// Build syntax-highlighted lines with line numbers.
fn highlight_lines(
    content: &str,
    syntax: &SyntaxReference,
    assets: &SyntectAssets,
    num_width: usize,
    max_lines: usize,
) -> Vec<Line<'static>> {
    let theme = &assets.theme_set.themes["base16-ocean.dark"];
    let mut highlighter = HighlightLines::new(syntax, theme);
    let gutter_style = Style::default().fg(Color::DarkGray);

    let mut result = Vec::new();
    for (i, line) in LinesWithEndings::from(content).enumerate() {
        if i >= max_lines {
            break;
        }
        let gutter = Span::styled(
            format!("{:>width$} │ ", i + 1, width = num_width),
            gutter_style,
        );
        let mut spans = vec![gutter];

        match highlighter.highlight_line(line, &assets.syntax_set) {
            Ok(ranges) => {
                for (style, text) in ranges {
                    let span = crate::tui::syntect_convert::into_span((style, text));
                    let s = span.content.trim_end_matches('\n');
                    if !s.is_empty() {
                        spans.push(Span::styled(s.to_owned(), span.style));
                    }
                }
            }
            Err(_) => {
                // Fallback: plain text for this line
                let s = line.trim_end_matches('\n');
                spans.push(Span::raw(s.to_owned()));
            }
        }
        result.push(Line::from(spans));
    }
    result
}

/// Format a byte size as a human-readable string.
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}
