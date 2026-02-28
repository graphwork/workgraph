use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use anyhow::Result;
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

use crate::commands::viz::{VizOptions, VizOutput};
use workgraph::graph::{parse_token_usage_live, Status, TokenUsage};
use workgraph::parser::load_graph;
use workgraph::{AgentRegistry, AgentStatus};

/// Task status counts for the status bar.
#[derive(Default)]
pub struct TaskCounts {
    pub total: usize,
    pub done: usize,
    pub open: usize,
    pub in_progress: usize,
    pub failed: usize,
    pub blocked: usize,
}

/// A single fuzzy match result for a line.
pub struct FuzzyLineMatch {
    /// Index into the original `lines`/`plain_lines` arrays.
    pub line_idx: usize,
    /// Fuzzy match score (higher = better). Used for sorting/ranking.
    #[allow(dead_code)]
    pub score: i64,
    /// Character positions within the plain line where the match occurs.
    /// These are *char* indices (not byte indices).
    pub char_positions: Vec<usize>,
}

/// Main application state for the viz viewer.
pub struct VizApp {
    /// Path to the workgraph directory.
    pub workgraph_dir: PathBuf,
    /// Viz options passed from CLI (--all, --status, --critical-path, etc.).
    viz_options: VizOptions,
    /// Whether the app should quit on next loop iteration.
    pub should_quit: bool,

    // ── Viz content ──
    /// Raw lines from `wg viz` output (may contain ANSI color codes).
    pub lines: Vec<String>,
    /// Stripped lines (no ANSI) for search matching and width calculation.
    pub plain_lines: Vec<String>,
    /// Sanitized lines for search — box-drawing/arrow chars replaced with spaces.
    search_lines: Vec<String>,
    /// Maximum line width in plain content (for horizontal scroll bounds).
    pub max_line_width: usize,

    // ── Viewport scroll ──
    pub scroll: ViewportScroll,

    // ── Search / Filter ──
    /// Whether the user is currently typing a search query.
    pub search_active: bool,
    /// The current search input buffer.
    pub search_input: String,
    /// Lines that fuzzy-match the current query, with scores and positions.
    pub fuzzy_matches: Vec<FuzzyLineMatch>,
    /// Index into `fuzzy_matches` for the currently focused match.
    pub current_match: Option<usize>,
    /// When filter is active, indices of original lines that are visible.
    /// `None` means show all lines (no filter).
    pub filtered_indices: Option<Vec<usize>>,
    /// The fuzzy matcher instance (reused across searches).
    matcher: SkimMatcherV2,

    // ── Task stats ──
    pub task_counts: TaskCounts,
    /// Aggregate token usage across all tasks in the graph.
    pub total_usage: TokenUsage,
    /// Per-task token usage keyed by task ID (for computing visible-task totals).
    pub task_token_map: HashMap<String, TokenUsage>,

    // ── Token display toggle ──
    /// When true, show total workgraph token usage; when false, show visible-tasks only.
    pub show_total_tokens: bool,

    // ── Help overlay ──
    pub show_help: bool,

    // ── Mouse capture ──
    /// Whether mouse capture is currently enabled.
    pub mouse_enabled: bool,

    // ── Jump target (transient highlight after Enter) ──
    /// After pressing Enter on a search match, stores (original_line_index, when_set).
    /// Render code applies a transient yellow highlight that fades after ~2 seconds.
    pub jump_target: Option<(usize, Instant)>,

    // ── Task selection / edge tracing ──
    /// Ordered list of task IDs as they appear in the viz output (top to bottom).
    pub task_order: Vec<String>,
    /// Map from task ID to its line index in the viz output.
    pub node_line_map: HashMap<String, usize>,
    /// Forward edges: task_id → dependent task IDs.
    pub forward_edges: HashMap<String, Vec<String>>,
    /// Reverse edges: task_id → dependency task IDs.
    pub reverse_edges: HashMap<String, Vec<String>>,
    /// Currently selected task index into `task_order`.
    pub selected_task_idx: Option<usize>,
    /// Transitive upstream (dependency) task IDs of the selected task.
    pub upstream_set: HashSet<String>,
    /// Transitive downstream (dependent) task IDs of the selected task.
    pub downstream_set: HashSet<String>,
    /// Set of line indices that belong to upstream edges (for coloring tree/arc connectors).
    pub upstream_lines: HashSet<usize>,
    /// Set of line indices that belong to downstream edges.
    pub downstream_lines: HashSet<usize>,

    // ── Live refresh ──
    /// Last observed modification time of graph.jsonl.
    last_graph_mtime: Option<SystemTime>,
    /// Monotonic instant of last data refresh.
    pub last_refresh: Instant,
    /// Display string for last refresh time (HH:MM:SS).
    pub last_refresh_display: String,
    /// Refresh interval.
    refresh_interval: std::time::Duration,
}

/// Scroll state for a 2D viewport.
pub struct ViewportScroll {
    /// First visible line index (vertical offset into the visible set).
    pub offset_y: usize,
    /// First visible column index (horizontal offset).
    pub offset_x: usize,
    /// Total content height in lines (filtered count when filter active).
    pub content_height: usize,
    /// Total content width in columns.
    pub content_width: usize,
    /// Viewport height (set each frame from terminal size).
    pub viewport_height: usize,
    /// Viewport width (set each frame from terminal size).
    pub viewport_width: usize,
}

impl VizApp {
    /// Create a new VizApp.
    ///
    /// `mouse_override`: `Some(false)` forces mouse off (--no-mouse),
    /// `None` means auto-detect (disable in tmux split panes).
    pub fn new(workgraph_dir: PathBuf, viz_options: VizOptions, mouse_override: Option<bool>) -> Self {
        let mouse_enabled = match mouse_override {
            Some(v) => v,
            None => !detect_tmux_split(),
        };
        let graph_mtime = std::fs::metadata(workgraph_dir.join("graph.jsonl"))
            .and_then(|m| m.modified())
            .ok();
        let mut app = Self {
            workgraph_dir,
            viz_options,
            should_quit: false,
            lines: Vec::new(),
            plain_lines: Vec::new(),
            search_lines: Vec::new(),
            max_line_width: 0,
            scroll: ViewportScroll::new(),
            search_active: false,
            search_input: String::new(),
            fuzzy_matches: Vec::new(),
            current_match: None,
            filtered_indices: None,
            matcher: SkimMatcherV2::default(),
            task_counts: TaskCounts::default(),
            total_usage: TokenUsage {
                cost_usd: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            task_token_map: HashMap::new(),
            show_total_tokens: false,
            show_help: false,
            mouse_enabled,
            jump_target: None,
            task_order: Vec::new(),
            node_line_map: HashMap::new(),
            forward_edges: HashMap::new(),
            reverse_edges: HashMap::new(),
            selected_task_idx: None,
            upstream_set: HashSet::new(),
            downstream_set: HashSet::new(),
            upstream_lines: HashSet::new(),
            downstream_lines: HashSet::new(),
            last_graph_mtime: graph_mtime,
            last_refresh: Instant::now(),
            last_refresh_display: chrono::Local::now().format("%H:%M:%S").to_string(),
            refresh_interval: std::time::Duration::from_millis(1500),
        };
        app.load_viz();
        app.load_stats();
        app
    }

    /// Load viz output by calling the viz module directly.
    pub fn load_viz(&mut self) {
        match self.generate_viz() {
            Ok(viz_output) => {
                self.lines = viz_output.text.lines().map(String::from).collect();
                self.plain_lines = self
                    .lines
                    .iter()
                    .map(|l| {
                        String::from_utf8(strip_ansi_escapes::strip(l.as_bytes())).unwrap_or_default()
                    })
                    .collect();
                self.search_lines = self
                    .plain_lines
                    .iter()
                    .map(|l| sanitize_for_search(l))
                    .collect();
                self.max_line_width =
                    self.plain_lines.iter().map(|l| l.len()).max().unwrap_or(0);

                // Store graph metadata for interactive edge tracing.
                self.node_line_map = viz_output.node_line_map;
                self.task_order = viz_output.task_order;
                self.forward_edges = viz_output.forward_edges;
                self.reverse_edges = viz_output.reverse_edges;

                // Preserve selection if possible (e.g., after refresh).
                if let Some(idx) = self.selected_task_idx {
                    if idx >= self.task_order.len() {
                        self.selected_task_idx = if self.task_order.is_empty() {
                            None
                        } else {
                            Some(self.task_order.len() - 1)
                        };
                    }
                } else if !self.task_order.is_empty() {
                    // Default to first task on initial load.
                    self.selected_task_idx = Some(0);
                }
                self.recompute_trace();

                self.update_scroll_bounds();
            }
            Err(_) => {
                self.lines = vec!["(error loading graph)".to_string()];
                self.plain_lines = self.lines.clone();
                self.search_lines = self.plain_lines.clone();
                self.max_line_width = self.lines[0].len();
                self.task_order.clear();
                self.node_line_map.clear();
                self.forward_edges.clear();
                self.reverse_edges.clear();
                self.selected_task_idx = None;
                self.upstream_set.clear();
                self.downstream_set.clear();
                self.upstream_lines.clear();
                self.downstream_lines.clear();
                self.update_scroll_bounds();
            }
        }
    }

    fn generate_viz(&self) -> Result<VizOutput> {
        crate::commands::viz::generate_viz_output(&self.workgraph_dir, &self.viz_options)
    }

    /// Update scroll content bounds based on current filter state.
    pub fn update_scroll_bounds(&mut self) {
        let height = match &self.filtered_indices {
            Some(indices) => indices.len(),
            None => self.lines.len(),
        };
        self.scroll.content_height = height;
        self.scroll.content_width = self.max_line_width;
        self.scroll.clamp();
    }

    /// Get the number of visible lines (filtered or all).
    pub fn visible_line_count(&self) -> usize {
        match &self.filtered_indices {
            Some(indices) => indices.len(),
            None => self.lines.len(),
        }
    }

    /// Map a visible row index to an original line index.
    pub fn visible_to_original(&self, visible_idx: usize) -> usize {
        match &self.filtered_indices {
            Some(indices) => indices.get(visible_idx).copied().unwrap_or(0),
            None => visible_idx,
        }
    }

    /// Map an original line index to its position in the visible set.
    fn original_to_visible(&self, orig_idx: usize) -> Option<usize> {
        match &self.filtered_indices {
            Some(indices) => indices.iter().position(|&i| i == orig_idx),
            None => {
                if orig_idx < self.lines.len() {
                    Some(orig_idx)
                } else {
                    None
                }
            }
        }
    }

    // ── Task selection / edge tracing ──

    /// Move task selection to the previous task in the viz order.
    pub fn select_prev_task(&mut self) {
        if self.task_order.is_empty() {
            return;
        }
        let idx = match self.selected_task_idx {
            Some(0) => self.task_order.len() - 1, // wrap around
            Some(i) => i - 1,
            None => 0,
        };
        self.selected_task_idx = Some(idx);
        self.recompute_trace();
        self.scroll_to_selected_task();
    }

    /// Move task selection to the next task in the viz order.
    pub fn select_next_task(&mut self) {
        if self.task_order.is_empty() {
            return;
        }
        let idx = match self.selected_task_idx {
            Some(i) if i + 1 >= self.task_order.len() => 0, // wrap around
            Some(i) => i + 1,
            None => 0,
        };
        self.selected_task_idx = Some(idx);
        self.recompute_trace();
        self.scroll_to_selected_task();
    }

    /// Recompute the transitive upstream/downstream sets and line mappings
    /// based on the currently selected task.
    pub fn recompute_trace(&mut self) {
        self.upstream_set.clear();
        self.downstream_set.clear();
        self.upstream_lines.clear();
        self.downstream_lines.clear();

        let selected_id = match self.selected_task_idx {
            Some(idx) => match self.task_order.get(idx) {
                Some(id) => id.clone(),
                None => return,
            },
            None => return,
        };

        // Compute transitive upstream (dependencies) via BFS on reverse_edges.
        {
            let mut queue = std::collections::VecDeque::new();
            for dep in self.reverse_edges.get(&selected_id).into_iter().flatten() {
                if self.upstream_set.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
            while let Some(id) = queue.pop_front() {
                for dep in self.reverse_edges.get(&id).into_iter().flatten() {
                    if self.upstream_set.insert(dep.clone()) {
                        queue.push_back(dep.clone());
                    }
                }
            }
        }

        // Compute transitive downstream (dependents) via BFS on forward_edges.
        {
            let mut queue = std::collections::VecDeque::new();
            for dep in self.forward_edges.get(&selected_id).into_iter().flatten() {
                if self.downstream_set.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
            while let Some(id) = queue.pop_front() {
                for dep in self.forward_edges.get(&id).into_iter().flatten() {
                    if self.downstream_set.insert(dep.clone()) {
                        queue.push_back(dep.clone());
                    }
                }
            }
        }

        // Build line sets for coloring connectors between nodes.
        // For upstream: all lines between the selected node and each upstream node.
        let selected_line = self.node_line_map.get(&selected_id).copied();
        if let Some(sel_line) = selected_line {
            for id in &self.upstream_set {
                if let Some(&line) = self.node_line_map.get(id) {
                    let (lo, hi) = if line < sel_line { (line, sel_line) } else { (sel_line, line) };
                    for l in lo..=hi {
                        self.upstream_lines.insert(l);
                    }
                }
            }
            for id in &self.downstream_set {
                if let Some(&line) = self.node_line_map.get(id) {
                    let (lo, hi) = if line < sel_line { (line, sel_line) } else { (sel_line, line) };
                    for l in lo..=hi {
                        self.downstream_lines.insert(l);
                    }
                }
            }
        }
    }

    /// Scroll the viewport so the selected task is visible.
    fn scroll_to_selected_task(&mut self) {
        let task_id = match self.selected_task_idx.and_then(|i| self.task_order.get(i)) {
            Some(id) => id,
            None => return,
        };
        let orig_line = match self.node_line_map.get(task_id) {
            Some(&line) => line,
            None => return,
        };
        if let Some(visible_pos) = self.original_to_visible(orig_line) {
            if visible_pos < self.scroll.offset_y
                || visible_pos >= self.scroll.offset_y + self.scroll.viewport_height
            {
                let half = self.scroll.viewport_height / 2;
                self.scroll.offset_y = visible_pos.saturating_sub(half);
                self.scroll.clamp();
            }
        }
    }

    /// Get the currently selected task ID, if any.
    pub fn selected_task_id(&self) -> Option<&str> {
        self.selected_task_idx
            .and_then(|i| self.task_order.get(i))
            .map(|s| s.as_str())
    }

    // ── Search ──

    /// Called on every keystroke while search is active.
    /// Performs incremental fuzzy matching and updates the filter.
    pub fn update_search(&mut self) {
        let query = &self.search_input;
        if query.is_empty() {
            self.fuzzy_matches.clear();
            self.current_match = None;
            self.filtered_indices = None;
            self.update_scroll_bounds();
            return;
        }

        // Run fuzzy matching on sanitized lines (box-drawing chars stripped).
        self.fuzzy_matches.clear();
        for (i, search_line) in self.search_lines.iter().enumerate() {
            if let Some((score, indices)) = self.matcher.fuzzy_indices(search_line, query) {
                // `indices` are byte positions — convert to char positions.
                let char_positions = byte_positions_to_char_positions(search_line, &indices);
                self.fuzzy_matches.push(FuzzyLineMatch {
                    line_idx: i,
                    score,
                    char_positions,
                });
            }
        }

        // Sort by score descending for match navigation order.
        // But keep original line order for the match index (navigate top-to-bottom).
        // fuzzy_matches are already in line order since we iterate lines sequentially.

        // Build filtered view: matching lines + their tree ancestors + section context.
        self.filtered_indices = Some(compute_filtered_indices(
            &self.plain_lines,
            &self.fuzzy_matches,
        ));

        self.update_scroll_bounds();

        // Set current match to the first match.
        if !self.fuzzy_matches.is_empty() {
            self.current_match = Some(0);
            self.scroll_to_current_match();
        } else {
            self.current_match = None;
        }
    }

    /// Accept the current search (Enter key). Exit search mode, show all lines,
    /// keep match highlights and viewport position (vim-style search).
    pub fn accept_search(&mut self) {
        self.search_active = false;
        self.filtered_indices = None;
        self.update_scroll_bounds();
        // Keep search_input, fuzzy_matches, current_match for highlights + navigation.
    }

    /// Accept search and jump to the current match with a transient highlight.
    /// Called when the user presses Enter on a search match.
    pub fn accept_search_and_jump(&mut self) {
        // Capture the current match's original line index before clearing filter.
        let target_line = self.current_match_line();
        self.accept_search();

        if let Some(orig_line) = target_line {
            // Set the transient highlight target.
            self.jump_target = Some((orig_line, Instant::now()));

            // Scroll to center on the target line in the full (unfiltered) view.
            let half = self.scroll.viewport_height / 2;
            self.scroll.offset_y = orig_line.saturating_sub(half);
            self.scroll.clamp();
        }
    }

    /// Jump to the next search match.
    pub fn next_match(&mut self) {
        if self.fuzzy_matches.is_empty() {
            return;
        }
        let next = match self.current_match {
            Some(idx) => (idx + 1) % self.fuzzy_matches.len(),
            None => 0,
        };
        self.current_match = Some(next);
        self.scroll_to_current_match();
    }

    /// Jump to the previous search match.
    pub fn prev_match(&mut self) {
        if self.fuzzy_matches.is_empty() {
            return;
        }
        let prev = match self.current_match {
            Some(0) => self.fuzzy_matches.len() - 1,
            Some(idx) => idx - 1,
            None => self.fuzzy_matches.len() - 1,
        };
        self.current_match = Some(prev);
        self.scroll_to_current_match();
    }

    /// Clear the search state entirely, restoring the full unfiltered view.
    pub fn clear_search(&mut self) {
        self.search_active = false;
        self.search_input.clear();
        self.fuzzy_matches.clear();
        self.current_match = None;
        self.filtered_indices = None;
        self.update_scroll_bounds();
    }

    /// Return a human-readable search status string for the status bar.
    pub fn search_status(&self) -> String {
        if self.search_active {
            if self.search_input.is_empty() {
                "/".to_string()
            } else if self.fuzzy_matches.is_empty() {
                format!("/{} [no matches]", self.search_input)
            } else {
                let idx = self.current_match.unwrap_or(0);
                format!(
                    "/{} [{}/{}]",
                    self.search_input,
                    idx + 1,
                    self.fuzzy_matches.len()
                )
            }
        } else if !self.search_input.is_empty() && !self.fuzzy_matches.is_empty() {
            // Accepted search — highlights visible, navigating with n/N/Tab.
            let idx = self.current_match.unwrap_or(0);
            format!(
                "/{} [{}/{}]",
                self.search_input,
                idx + 1,
                self.fuzzy_matches.len()
            )
        } else {
            String::new()
        }
    }

    /// Check if any search/filter is active (for rendering decisions).
    pub fn has_active_search(&self) -> bool {
        !self.search_input.is_empty() && !self.fuzzy_matches.is_empty()
    }

    /// Get the fuzzy match info for an original line index, if any.
    pub fn match_for_line(&self, orig_idx: usize) -> Option<&FuzzyLineMatch> {
        self.fuzzy_matches.iter().find(|m| m.line_idx == orig_idx)
    }

    /// Get the original line index of the current match (for highlight).
    pub fn current_match_line(&self) -> Option<usize> {
        self.current_match
            .and_then(|idx| self.fuzzy_matches.get(idx))
            .map(|m| m.line_idx)
    }

    /// Scroll the viewport so the current match is visible (centered).
    fn scroll_to_current_match(&mut self) {
        if let Some(match_idx) = self.current_match {
            let orig_line = self.fuzzy_matches[match_idx].line_idx;
            // Convert to visible position.
            if let Some(visible_pos) = self.original_to_visible(orig_line)
                && (visible_pos < self.scroll.offset_y
                    || visible_pos >= self.scroll.offset_y + self.scroll.viewport_height)
                {
                    let half = self.scroll.viewport_height / 2;
                    self.scroll.offset_y = visible_pos.saturating_sub(half);
                    self.scroll.clamp();
                }
        }
    }

    // ── Refresh ──

    /// Re-run search on new content after a graph refresh.
    fn rerun_search(&mut self) {
        if self.search_input.is_empty() {
            return;
        }
        // Re-run the fuzzy match with the current query.
        self.fuzzy_matches.clear();
        for (i, search_line) in self.search_lines.iter().enumerate() {
            if let Some((score, indices)) = self.matcher.fuzzy_indices(search_line, &self.search_input) {
                let char_positions = byte_positions_to_char_positions(search_line, &indices);
                self.fuzzy_matches.push(FuzzyLineMatch {
                    line_idx: i,
                    score,
                    char_positions,
                });
            }
        }
        if self.search_active {
            self.filtered_indices = Some(compute_filtered_indices(
                &self.plain_lines,
                &self.fuzzy_matches,
            ));
        }
        self.update_scroll_bounds();
        // Try to preserve current match position.
        if !self.fuzzy_matches.is_empty() {
            if self.current_match.is_none()
                || self.current_match.unwrap() >= self.fuzzy_matches.len()
            {
                self.current_match = Some(0);
            }
        } else {
            self.current_match = None;
        }
    }

    /// Load task counts and token usage from the graph + live agent output.
    pub fn load_stats(&mut self) {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = match load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => {
                self.task_counts = TaskCounts::default();
                self.total_usage = TokenUsage {
                    cost_usd: 0.0,
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                };
                self.task_token_map.clear();
                return;
            }
        };

        let mut counts = TaskCounts::default();
        let mut total_usage = TokenUsage {
            cost_usd: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        let mut task_token_map: HashMap<String, TokenUsage> = HashMap::new();

        // Build a map of agent_id -> live token usage for in-progress agents
        let mut live_agent_usage: HashMap<String, TokenUsage> = HashMap::new();
        if let Ok(registry) = AgentRegistry::load(&self.workgraph_dir) {
            for (id, agent) in &registry.agents {
                if agent.status != AgentStatus::Working || agent.output_file.is_empty() {
                    continue;
                }
                let path = std::path::Path::new(&agent.output_file);
                if let Some(usage) = parse_token_usage_live(path) {
                    live_agent_usage.insert(id.clone(), usage);
                }
            }
        }

        for task in graph.tasks() {
            counts.total += 1;
            match task.status {
                Status::Done => counts.done += 1,
                Status::Open => counts.open += 1,
                Status::InProgress => counts.in_progress += 1,
                Status::Failed => counts.failed += 1,
                Status::Blocked => counts.blocked += 1,
                Status::Abandoned => counts.done += 1, // count with done
            }

            // Use stored token_usage if available, otherwise check live agent data
            let usage = task.token_usage.as_ref().or_else(|| {
                task.assigned.as_ref().and_then(|aid| live_agent_usage.get(aid))
            });

            if let Some(usage) = usage {
                total_usage.accumulate(usage);
                task_token_map.insert(task.id.clone(), usage.clone());
            }
        }

        self.task_counts = counts;
        self.total_usage = total_usage;
        self.task_token_map = task_token_map;
    }

    /// Check if the graph has changed on disk and refresh if needed.
    pub fn maybe_refresh(&mut self) {
        if self.last_refresh.elapsed() < self.refresh_interval {
            return;
        }

        let current_mtime = std::fs::metadata(self.workgraph_dir.join("graph.jsonl"))
            .and_then(|m| m.modified())
            .ok();

        let graph_changed = current_mtime != self.last_graph_mtime;
        let needs_token_refresh = self.task_counts.in_progress > 0;

        if graph_changed || needs_token_refresh {
            if graph_changed {
                self.last_graph_mtime = current_mtime;
                self.load_viz();
                if !self.search_input.is_empty() {
                    self.rerun_search();
                }
            }
            self.load_stats();
            self.last_refresh_display = chrono::Local::now().format("%H:%M:%S").to_string();
        }

        self.last_refresh = Instant::now();
    }

    /// Cycle through layout modes (tree ↔ diamond).
    pub fn cycle_layout(&mut self) {
        use crate::commands::viz::LayoutMode;
        self.viz_options.layout = match self.viz_options.layout {
            LayoutMode::Tree => LayoutMode::Diamond,
            LayoutMode::Diamond => LayoutMode::Tree,
        };
        self.force_refresh();
    }

    /// Get the current layout mode name for display.
    #[allow(dead_code)]
    pub fn layout_name(&self) -> &'static str {
        use crate::commands::viz::LayoutMode;
        match self.viz_options.layout {
            LayoutMode::Tree => "tree",
            LayoutMode::Diamond => "diamond",
        }
    }

    /// Compute aggregate token usage for tasks currently visible on screen.
    /// Extracts task IDs from the plain_lines visible in the viewport.
    pub fn visible_token_usage(&self) -> TokenUsage {
        let mut usage = TokenUsage {
            cost_usd: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        // Collect unique task IDs from all visible lines (not just viewport)
        let visible_count = self.visible_line_count();
        let mut seen = HashSet::new();
        for visible_idx in 0..visible_count {
            let orig_idx = self.visible_to_original(visible_idx);
            if let Some(plain) = self.plain_lines.get(orig_idx)
                && let Some(task_id) = extract_task_id(plain)
                    && seen.insert(task_id.clone())
                        && let Some(task_usage) = self.task_token_map.get(&task_id) {
                            usage.accumulate(task_usage);
                        }
        }
        usage
    }

    /// Toggle mouse capture on/off.
    pub fn toggle_mouse(&mut self) {
        self.mouse_enabled = !self.mouse_enabled;
    }

    /// Force an immediate refresh (manual `r` key).
    pub fn force_refresh(&mut self) {
        self.last_graph_mtime = std::fs::metadata(self.workgraph_dir.join("graph.jsonl"))
            .and_then(|m| m.modified())
            .ok();
        self.load_viz();
        if !self.search_input.is_empty() {
            self.rerun_search();
        }
        self.load_stats();
        self.last_refresh_display = chrono::Local::now().format("%H:%M:%S").to_string();
        self.last_refresh = Instant::now();
    }
}

/// Detect if we're running inside a tmux split pane.
///
/// Compares the terminal size (from crossterm) with the tmux window size.
/// If the terminal is smaller than the tmux window, we're in a split pane
/// and mouse capture should be disabled by default (tmux needs mouse events
/// for pane selection/resize).
fn detect_tmux_split() -> bool {
    // Only applies if TMUX env var is set
    if std::env::var("TMUX").is_err() {
        return false;
    }

    // Get terminal size from crossterm
    let (term_cols, term_rows) = match crossterm::terminal::size() {
        Ok(size) => size,
        Err(_) => return false,
    };

    // Get tmux window size via `tmux display-message -p '#{window_width} #{window_height}'`
    let output = match std::process::Command::new("tmux")
        .args(["display-message", "-p", "#{window_width} #{window_height}"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return false,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = stdout.trim().split_whitespace().collect();
    if parts.len() != 2 {
        return false;
    }

    let (tmux_cols, tmux_rows) = match (parts[0].parse::<u16>(), parts[1].parse::<u16>()) {
        (Ok(c), Ok(r)) => (c, r),
        _ => return false,
    };

    // If terminal is smaller than tmux window, we're in a split
    term_cols < tmux_cols || term_rows < tmux_rows
}

// ── Tree-aware filtering ──

/// Determine the "indent level" of a line: the char-index of the first alphanumeric character.
/// Returns `None` for lines with no alphanumeric characters (blank, pure box-drawing, etc.).
fn line_indent_level(plain: &str) -> Option<usize> {
    plain
        .chars()
        .enumerate()
        .find(|(_, c)| c.is_alphanumeric())
        .map(|(i, _)| i)
}

/// Check if a line is a summary/separator line (e.g., "  ╌╌ 12 tasks ╌╌").
fn is_summary_line(plain: &str) -> bool {
    plain.trim().starts_with('╌')
}

/// Compute the set of visible line indices given the fuzzy matches.
/// Includes matching lines, their tree ancestors, and section context.
fn compute_filtered_indices(
    plain_lines: &[String],
    fuzzy_matches: &[FuzzyLineMatch],
) -> Vec<usize> {
    if fuzzy_matches.is_empty() {
        return Vec::new();
    }

    let matching_lines: HashSet<usize> = fuzzy_matches.iter().map(|m| m.line_idx).collect();

    // Parse sections: each section is a group of non-empty lines,
    // separated by blank lines. The last non-blank line in a section
    // is typically a summary starting with ╌╌.
    let mut sections: Vec<(usize, usize)> = Vec::new(); // (start, end) inclusive
    let mut i = 0;
    while i < plain_lines.len() {
        // Skip blank lines between sections.
        if plain_lines[i].trim().is_empty() {
            i += 1;
            continue;
        }
        let start = i;
        while i < plain_lines.len() && !plain_lines[i].trim().is_empty() {
            i += 1;
        }
        sections.push((start, i - 1)); // end is inclusive
    }

    let mut visible: HashSet<usize> = HashSet::new();

    for &(sec_start, sec_end) in &sections {
        // Check if any line in this section matches.
        let section_has_match = (sec_start..=sec_end).any(|idx| matching_lines.contains(&idx));
        if !section_has_match {
            continue;
        }

        // For each matching line in this section, include it and its tree ancestors.
        for line_idx in sec_start..=sec_end {
            if !matching_lines.contains(&line_idx) {
                continue;
            }

            visible.insert(line_idx);

            // Walk backwards to find ancestor lines (lines with smaller indent).
            let match_indent = line_indent_level(&plain_lines[line_idx]);
            if match_indent.is_none() {
                continue;
            }
            let mut need_below = match_indent.unwrap();

            for ancestor_idx in (sec_start..line_idx).rev() {
                if is_summary_line(&plain_lines[ancestor_idx]) {
                    continue;
                }
                if let Some(indent) = line_indent_level(&plain_lines[ancestor_idx])
                    && indent < need_below {
                        visible.insert(ancestor_idx);
                        need_below = indent;
                        if indent == 0 {
                            break; // reached root
                        }
                    }
            }
        }

        // Always include the summary line for sections that have matches.
        if is_summary_line(&plain_lines[sec_end]) {
            visible.insert(sec_end);
        }
    }

    // Build sorted result. Insert blank lines between sections for readability.
    let mut result: Vec<usize> = visible.into_iter().collect();
    result.sort();
    result
}

/// Extract a task ID from a plain (ANSI-stripped) viz line.
/// Task lines look like: `  ├→ task-id  (status · tokens)` or `task-id  (status)`.
/// Returns None for non-task lines (summaries, blanks, box-drawing-only lines).
fn extract_task_id(plain: &str) -> Option<String> {
    // Skip summary/separator lines
    if is_summary_line(plain) {
        return None;
    }
    // Find the first alphanumeric/hyphen/underscore sequence (the task ID).
    // Task IDs consist of [a-zA-Z0-9_-].
    let trimmed = plain.trim_start();
    // Strip leading tree connectors (box-drawing + arrows + spaces)
    let after_connectors: &str = trimmed
        .trim_start_matches(|c: char| is_box_drawing(c) || c == ' ');
    if after_connectors.is_empty() {
        return None;
    }
    // The task ID is the first "word" — characters that are alphanumeric, hyphen, or underscore.
    let id: String = after_connectors
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if id.is_empty() {
        return None;
    }
    // Verify it looks like a task line: after the ID there should be whitespace then '('
    let rest = &after_connectors[id.len()..];
    if rest.trim_start().starts_with('(') {
        Some(id)
    } else {
        None
    }
}

/// Replace box-drawing and arrow characters with spaces so fuzzy search
/// doesn't match on visual decoration (├│─◀▶╌ etc.).
fn sanitize_for_search(line: &str) -> String {
    line.chars()
        .map(|c| if is_box_drawing(c) { ' ' } else { c })
        .collect()
}

pub(super) fn is_box_drawing(c: char) -> bool {
    matches!(
        c,
        '│' | '├'
            | '└'
            | '┌'
            | '┐'
            | '┘'
            | '─'
            | '╌'
            | '◀'
            | '▶'
            | '←'
            | '→'
            | '↓'
            | '↑'
            | '╭'
            | '╮'
            | '╯'
            | '╰'
            | '┼'
            | '┤'
            | '┬'
            | '┴'
            | '▼'
            | '▲'
            | '►'
            | '◄'
    )
}

/// Convert byte positions (from fuzzy_indices) to char positions for a given string.
fn byte_positions_to_char_positions(s: &str, byte_positions: &[usize]) -> Vec<usize> {
    if byte_positions.is_empty() {
        return Vec::new();
    }
    let byte_set: HashSet<usize> = byte_positions.iter().copied().collect();
    let mut char_positions = Vec::with_capacity(byte_positions.len());
    for (char_idx, (byte_idx, _)) in s.char_indices().enumerate() {
        if byte_set.contains(&byte_idx) {
            char_positions.push(char_idx);
        }
    }
    char_positions
}

impl ViewportScroll {
    pub fn new() -> Self {
        Self {
            offset_y: 0,
            offset_x: 0,
            content_height: 0,
            content_width: 0,
            viewport_height: 0,
            viewport_width: 0,
        }
    }

    pub fn scroll_up(&mut self, amount: usize) {
        self.offset_y = self.offset_y.saturating_sub(amount);
    }

    pub fn scroll_down(&mut self, amount: usize) {
        let max_y = self.content_height.saturating_sub(self.viewport_height);
        self.offset_y = (self.offset_y + amount).min(max_y);
    }

    pub fn scroll_left(&mut self, amount: usize) {
        self.offset_x = self.offset_x.saturating_sub(amount);
    }

    pub fn scroll_right(&mut self, amount: usize) {
        let max_x = self.content_width.saturating_sub(self.viewport_width);
        self.offset_x = (self.offset_x + amount).min(max_x);
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.viewport_height / 2);
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.viewport_height / 2);
    }

    pub fn go_top(&mut self) {
        self.offset_y = 0;
    }

    pub fn go_bottom(&mut self) {
        self.offset_y = self.content_height.saturating_sub(self.viewport_height);
    }

    /// Clamp scroll offset to valid range after content changes.
    pub fn clamp(&mut self) {
        let max_y = self.content_height.saturating_sub(self.viewport_height);
        self.offset_y = self.offset_y.min(max_y);
        let max_x = self.content_width.saturating_sub(self.viewport_width);
        self.offset_x = self.offset_x.min(max_x);
    }
}
