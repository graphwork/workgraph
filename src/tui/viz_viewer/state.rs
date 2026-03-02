use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Instant, SystemTime};

use anyhow::Result;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use ratatui::layout::Rect;

use crate::commands::viz::{VizOptions, VizOutput};
use workgraph::graph::{Status, TokenUsage, format_tokens, parse_token_usage_live};
use workgraph::parser::load_graph;
use workgraph::{AgentRegistry, AgentStatus};

// ══════════════════════════════════════════════════════════════════════════════
// Panel state types
// ══════════════════════════════════════════════════════════════════════════════

/// Which panel currently has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FocusedPanel {
    Graph,
    RightPanel,
}

/// Which tab is active in the right panel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RightPanelTab {
    Chat,    // 0
    Detail,  // 1
    Log,     // 2
    Messages,// 3
    Agency,  // 4
}

impl RightPanelTab {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Detail => "Detail",
            Self::Log => "Log",
            Self::Messages => "Msg",
            Self::Agency => "Agency",
        }
    }

    pub fn index(&self) -> usize {
        match self {
            Self::Chat => 0,
            Self::Detail => 1,
            Self::Log => 2,
            Self::Messages => 3,
            Self::Agency => 4,
        }
    }

    pub fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::Chat),
            1 => Some(Self::Detail),
            2 => Some(Self::Log),
            3 => Some(Self::Messages),
            4 => Some(Self::Agency),
            _ => None,
        }
    }

    pub fn next(&self) -> Self {
        Self::from_index((self.index() + 1) % 5).unwrap()
    }

    pub fn prev(&self) -> Self {
        Self::from_index((self.index() + 4) % 5).unwrap()
    }

    pub const ALL: [RightPanelTab; 5] = [
        Self::Chat,
        Self::Detail,
        Self::Log,
        Self::Messages,
        Self::Agency,
    ];
}

/// Sort mode for task ordering in the graph view.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    /// Default graph layout order (dependency tree, roots at top).
    Chronological,
    /// Reverse of default: newest/leaf tasks at bottom, viewport starts at bottom.
    ReverseChronological,
    /// Group tasks by status: in-progress first, then open/blocked, then done.
    StatusGrouped,
}

impl SortMode {
    pub fn cycle(&self) -> Self {
        match self {
            Self::Chronological => Self::ReverseChronological,
            Self::ReverseChronological => Self::StatusGrouped,
            Self::StatusGrouped => Self::Chronological,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Chronological => "Chrono ↓",
            Self::ReverseChronological => "Chrono ↑",
            Self::StatusGrouped => "Status",
        }
    }
}

/// HUD panel size preset.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HudSize {
    /// ~1/3 of terminal (default).
    Normal,
    /// ~2/3 of terminal (expanded).
    Expanded,
}

impl HudSize {
    pub fn cycle(&self) -> Self {
        match self {
            Self::Normal => Self::Expanded,
            Self::Expanded => Self::Normal,
        }
    }

    /// Side panel width percentage.
    pub fn side_percent(&self) -> u16 {
        match self {
            Self::Normal => 35,
            Self::Expanded => 65,
        }
    }

    /// Bottom panel height percentage.
    pub fn bottom_percent(&self) -> u16 {
        match self {
            Self::Normal => 40,
            Self::Expanded => 65,
        }
    }
}

/// Input modes — at most one is active at a time.
#[derive(Clone, PartialEq, Eq)]
pub enum InputMode {
    /// Normal navigation mode. Keys go to the focused panel.
    Normal,
    /// Search mode (/ key). Keys go to search input.
    Search,
    /// Chat input mode. Keys go to chat text input.
    ChatInput,
    /// Message tab input mode. Keys go to message text input.
    MessageInput,
    /// Task creation form. Keys go to form fields.
    TaskForm,
    /// Confirmation dialog (e.g., "Mark task done? y/n").
    Confirm(ConfirmAction),
    /// Text prompt dialog (e.g., fail reason, message text).
    TextPrompt(TextPromptAction),
}

/// What action the confirmation dialog is for.
#[derive(Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    MarkDone(String),  // task_id
    Retry(String),     // task_id
}

/// What action the text prompt dialog is for.
#[derive(Clone, PartialEq, Eq)]
pub enum TextPromptAction {
    MarkFailed(String),   // task_id
    SendMessage(String),  // task_id
    EditDescription(String), // task_id
}

/// State for the task creation form overlay.
pub struct TaskFormState {
    /// Which field is currently focused.
    pub active_field: TaskFormField,
    /// Title input buffer.
    pub title: String,
    /// Description input buffer (multiline).
    pub description: String,
    /// Dependency search input for fuzzy-finding tasks.
    pub dep_search: String,
    /// Selected dependency task IDs (--after).
    pub selected_deps: Vec<String>,
    /// Fuzzy search results for dependency selector.
    pub dep_matches: Vec<(String, String)>, // (task_id, title)
    /// Selected index in dependency match list.
    pub dep_match_idx: usize,
    /// Tags input buffer (comma-separated).
    pub tags: String,
    /// All available task IDs + titles for dependency search.
    pub all_tasks: Vec<(String, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaskFormField {
    Title,
    Description,
    Dependencies,
    Tags,
}

impl TaskFormField {
    pub fn next(&self) -> Self {
        match self {
            Self::Title => Self::Description,
            Self::Description => Self::Dependencies,
            Self::Dependencies => Self::Tags,
            Self::Tags => Self::Title,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            Self::Title => Self::Tags,
            Self::Description => Self::Title,
            Self::Dependencies => Self::Description,
            Self::Tags => Self::Dependencies,
        }
    }
}

impl TaskFormState {
    pub fn new(workgraph_dir: &std::path::Path) -> Self {
        // Load all tasks for dependency search.
        let graph_path = workgraph_dir.join("graph.jsonl");
        let all_tasks = match load_graph(&graph_path) {
            Ok(g) => g
                .tasks()
                .map(|t| (t.id.clone(), t.title.clone()))
                .collect(),
            Err(_) => Vec::new(),
        };
        Self {
            active_field: TaskFormField::Title,
            title: String::new(),
            description: String::new(),
            dep_search: String::new(),
            selected_deps: Vec::new(),
            dep_matches: Vec::new(),
            dep_match_idx: 0,
            tags: String::new(),
            all_tasks,
        }
    }

    /// Update fuzzy search results for dependency field.
    pub fn update_dep_search(&mut self) {
        if self.dep_search.is_empty() {
            self.dep_matches.clear();
            self.dep_match_idx = 0;
            return;
        }
        let matcher = SkimMatcherV2::default();
        let query = &self.dep_search;
        let mut matches: Vec<(i64, String, String)> = self
            .all_tasks
            .iter()
            .filter(|(id, _)| !self.selected_deps.contains(id))
            .filter_map(|(id, title)| {
                let search_str = format!("{} {}", id, title);
                matcher
                    .fuzzy_match(&search_str, query)
                    .map(|score| (score, id.clone(), title.clone()))
            })
            .collect();
        matches.sort_by(|a, b| b.0.cmp(&a.0));
        self.dep_matches = matches
            .into_iter()
            .take(8)
            .map(|(_, id, title)| (id, title))
            .collect();
        self.dep_match_idx = 0;
    }
}

/// State for the chat panel.
pub struct ChatState {
    /// Message history for display.
    pub messages: Vec<ChatMessage>,
    /// Current input buffer (may contain newlines from paste).
    pub input: String,
    /// Cursor position (byte offset) within `input`.
    pub cursor: usize,
    /// Scroll offset within the input box (visual line index from top).
    /// Used when input content exceeds the visible input area height.
    pub input_scroll: usize,
    /// Scroll offset in message history (lines from bottom; 0 = fully scrolled down).
    pub scroll: usize,
    /// Whether we're waiting for a coordinator response.
    pub awaiting_response: bool,
    /// Outbox cursor: last-read outbox message ID (for polling new messages).
    pub outbox_cursor: u64,
    /// Request ID of the last sent message (for correlating responses).
    pub last_request_id: Option<String>,
    /// Whether the service coordinator is currently active.
    pub coordinator_active: bool,
}

impl Default for ChatState {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            cursor: 0,
            input_scroll: 0,
            scroll: 0,
            awaiting_response: false,
            outbox_cursor: 0,
            last_request_id: None,
            coordinator_active: false,
        }
    }
}

pub struct ChatMessage {
    pub role: ChatRole,
    pub text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Coordinator,
    System,
}

/// State for the agent monitor panel.
pub struct AgentMonitorState {
    /// Agent entries loaded from the registry.
    pub agents: Vec<AgentMonitorEntry>,
    /// Scroll offset.
    pub scroll: usize,
}

impl Default for AgentMonitorState {
    fn default() -> Self {
        Self {
            agents: Vec::new(),
            scroll: 0,
        }
    }
}

pub struct AgentMonitorEntry {
    pub agent_id: String,
    pub task_id: Option<String>,
    pub task_title: Option<String>,
    pub status: AgentStatus,
    pub runtime_secs: Option<i64>,
    /// ISO 8601 start timestamp
    pub started_at: Option<String>,
    /// ISO 8601 completion timestamp (for Done/Failed/Dead agents)
    pub completed_at: Option<String>,
}

/// State for the log pane (now embedded as right panel tab 2).
pub struct LogPaneState {
    /// Scroll offset from the top of log content.
    pub scroll: usize,
    /// Whether auto-scroll (tail mode) is active — scroll to bottom on new content.
    pub auto_tail: bool,
    /// Whether to show raw JSON format (toggled by `J`).
    pub json_mode: bool,
    /// Cached rendered log lines for the currently selected task.
    pub rendered_lines: Vec<String>,
    /// Task ID these lines were rendered for (to detect staleness).
    pub task_id: Option<String>,
    /// Height of the log pane viewport (set each frame).
    pub viewport_height: usize,
}

impl Default for LogPaneState {
    fn default() -> Self {
        Self {
            scroll: 0,
            auto_tail: true,
            json_mode: false,
            rendered_lines: Vec::new(),
            task_id: None,
            viewport_height: 0,
        }
    }
}

/// State for the Messages panel (panel 3) — shows message queue for the selected task.
pub struct MessagesPanelState {
    /// Cached rendered log lines.
    pub rendered_lines: Vec<String>,
    /// Task ID these lines were rendered for (to detect staleness).
    pub task_id: Option<String>,
    /// Scroll offset.
    pub scroll: usize,
    /// Current input buffer for composing messages.
    pub input: String,
    /// Cursor position (byte offset) within `input`.
    pub cursor: usize,
}

impl Default for MessagesPanelState {
    fn default() -> Self {
        Self {
            rendered_lines: Vec::new(),
            task_id: None,
            scroll: 0,
            input: String::new(),
            cursor: 0,
        }
    }
}

/// A background command result received from a spawned thread.
pub struct CommandResult {
    pub success: bool,
    pub output: String,
    pub effect: CommandEffect,
}

/// What to do after a command completes.
#[derive(Clone)]
pub enum CommandEffect {
    /// Trigger a full graph refresh.
    Refresh,
    /// Show a notification message in the status bar.
    Notify(String),
    /// Refresh + notify.
    RefreshAndNotify(String),
    /// A chat response arrived from `wg chat` — output is the coordinator's response text.
    /// The String is the request_id for correlation.
    ChatResponse(String),
}

/// Text prompt state (shared input buffer for fail reason, message, etc.)
pub struct TextPromptState {
    pub input: String,
}

/// Loaded detail for the HUD panel showing info about the selected task.
#[derive(Default)]
pub struct HudDetail {
    /// Task ID this detail was loaded for (to detect stale data).
    pub task_id: String,
    /// All content lines assembled for rendering (with section headers).
    pub rendered_lines: Vec<String>,
}

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

    // ── Layout areas (set each frame by the renderer, for mouse hit-testing) ──
    /// The graph/viz content area from the last render frame.
    pub last_graph_area: Rect,
    /// The full right panel area (including border) from the last render frame.
    pub last_right_panel_area: Rect,
    /// The tab bar area inside the right panel from the last render frame.
    pub last_tab_bar_area: Rect,
    /// The content area inside the right panel (below tab bar) from the last render frame.
    pub last_right_content_area: Rect,

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
    /// Whether edge trace highlighting is visible (toggled by Tab).
    pub trace_visible: bool,
    /// Transitive upstream (dependency) task IDs of the selected task.
    pub upstream_set: HashSet<String>,
    /// Transitive downstream (dependent) task IDs of the selected task.
    pub downstream_set: HashSet<String>,
    /// Per-character edge map: (line, visible_column) → list of (source_id, target_id).
    /// Maps edge/connector characters to the graph edge(s) they represent.
    /// Shared arc column positions may carry multiple edges.
    pub char_edge_map: std::collections::HashMap<(usize, usize), Vec<(String, String)>>,
    /// Cycle membership from VizOutput: task_id → set of SCC members.
    cycle_members: HashMap<String, HashSet<String>>,
    /// Set of task IDs in the same SCC as the currently selected task.
    /// Empty if the selected task is not in any cycle.
    pub cycle_set: HashSet<String>,

    // ── HUD (info panel) ──
    /// Loaded HUD detail for the currently selected task.
    pub hud_detail: Option<HudDetail>,
    /// Scroll offset within the HUD panel (vertical).
    pub hud_scroll: usize,
    /// Total wrapped line count in the detail panel (set by renderer each frame).
    pub hud_wrapped_line_count: usize,
    /// Viewport height of the detail panel (set by renderer each frame).
    pub hud_detail_viewport_height: usize,

    // ── Multi-panel layout ──
    /// Whether the right panel is visible (toggle with `\`).
    pub right_panel_visible: bool,
    /// Which panel has keyboard focus.
    pub focused_panel: FocusedPanel,
    /// Active tab in the right panel.
    pub right_panel_tab: RightPanelTab,
    /// Right panel width as percentage of terminal width (default 35).
    pub right_panel_percent: u16,
    /// HUD panel size preset (Normal = ~1/3, Expanded = ~2/3).
    pub hud_size: HudSize,
    /// Current input mode.
    pub input_mode: InputMode,

    // ── Task form ──
    /// Task creation form state (populated when form is open).
    pub task_form: Option<TaskFormState>,

    // ── Text prompt ──
    /// Text prompt input buffer (for fail reason, message, etc.)
    pub text_prompt: TextPromptState,

    // ── Chat state ──
    pub chat: ChatState,

    // ── Agent monitor state ──
    pub agent_monitor: AgentMonitorState,

    // ── Log pane state (now embedded as panel 2) ──
    pub log_pane: LogPaneState,

    // ── Messages panel state (panel 3) ──
    pub messages_panel: MessagesPanelState,

    // ── Command queue ──
    /// Channel receiver for background command results.
    pub cmd_rx: mpsc::Receiver<CommandResult>,
    /// Channel sender (cloned into background threads).
    pub cmd_tx: mpsc::Sender<CommandResult>,
    /// Notification message to display (transient, cleared after a few seconds).
    pub notification: Option<(String, Instant)>,

    // ── Double-tap detection ──
    /// Timestamp of the last Tab key press, for double-tap recenter detection.
    pub last_tab_press: Option<Instant>,

    // ── Sort mode ──
    /// Current sort mode for task ordering in the graph view.
    pub sort_mode: SortMode,

    // ── Smart-follow ──
    /// Whether the user was at/near the bottom of the viewport before the last refresh.
    /// Used to auto-scroll to bottom when new content appears.
    pub smart_follow_active: bool,
    /// Whether this is the initial load (first load scrolls to bottom by default).
    initial_load: bool,

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
    pub fn new(
        workgraph_dir: PathBuf,
        viz_options: VizOptions,
        mouse_override: Option<bool>,
    ) -> Self {
        let mouse_enabled = match mouse_override {
            Some(v) => v,
            None => !detect_tmux_split(),
        };
        let graph_mtime = std::fs::metadata(workgraph_dir.join("graph.jsonl"))
            .and_then(|m| m.modified())
            .ok();
        let (cmd_tx, cmd_rx) = mpsc::channel();
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
            last_graph_area: Rect::default(),
            last_right_panel_area: Rect::default(),
            last_tab_bar_area: Rect::default(),
            last_right_content_area: Rect::default(),
            jump_target: None,
            task_order: Vec::new(),
            node_line_map: HashMap::new(),
            forward_edges: HashMap::new(),
            reverse_edges: HashMap::new(),
            selected_task_idx: None,
            trace_visible: true,
            upstream_set: HashSet::new(),
            downstream_set: HashSet::new(),
            char_edge_map: std::collections::HashMap::new(),
            cycle_members: HashMap::new(),
            cycle_set: HashSet::new(),
            hud_detail: None,
            hud_scroll: 0,
            hud_wrapped_line_count: 0,
            hud_detail_viewport_height: 0,
            right_panel_visible: true,
            focused_panel: FocusedPanel::Graph,
            right_panel_tab: RightPanelTab::Detail,
            right_panel_percent: 35,
            hud_size: HudSize::Normal,
            input_mode: InputMode::Normal,
            task_form: None,
            text_prompt: TextPromptState {
                input: String::new(),
            },
            chat: ChatState::default(),
            agent_monitor: AgentMonitorState::default(),
            log_pane: LogPaneState::default(),
            messages_panel: MessagesPanelState::default(),
            cmd_rx,
            cmd_tx,
            notification: None,
            last_tab_press: None,
            sort_mode: SortMode::Chronological,
            smart_follow_active: true,
            initial_load: true,
            last_graph_mtime: graph_mtime,
            last_refresh: Instant::now(),
            last_refresh_display: chrono::Local::now().format("%H:%M:%S").to_string(),
            refresh_interval: std::time::Duration::from_millis(1500),
        };
        app.load_viz();
        app.load_stats();
        app.load_agent_monitor();
        app.check_coordinator_status();
        app.load_chat_history();
        app
    }

    /// Load viz output by calling the viz module directly.
    pub fn load_viz(&mut self) {
        // Smart-follow: snapshot whether the user is at the bottom before reloading.
        let was_at_bottom = self.smart_follow_active || self.initial_load;

        // Anchor on the selected task's RELATIVE position within the viewport.
        // This keeps the task visually stable even when lines shift above it.
        let old_offset_y = self.scroll.offset_y;
        let old_selected_id = self.selected_task_id().map(String::from);
        let old_relative_pos: Option<isize> = old_selected_id.as_ref().and_then(|id| {
            let orig_line = *self.node_line_map.get(id)?;
            let visible_pos = self.original_to_visible(orig_line)?;
            Some(visible_pos as isize - old_offset_y as isize)
        });

        match self.generate_viz() {
            Ok(viz_output) => {
                self.lines = viz_output
                    .text
                    .lines()
                    .map(String::from)
                    .filter(|l| {
                        let stripped = String::from_utf8(strip_ansi_escapes::strip(l.as_bytes()))
                            .unwrap_or_default();
                        !stripped.trim_start().starts_with("Legend:")
                    })
                    .collect();
                self.plain_lines = self
                    .lines
                    .iter()
                    .map(|l| {
                        String::from_utf8(strip_ansi_escapes::strip(l.as_bytes()))
                            .unwrap_or_default()
                    })
                    .collect();
                self.search_lines = self
                    .plain_lines
                    .iter()
                    .map(|l| sanitize_for_search(l))
                    .collect();
                self.max_line_width = self.plain_lines.iter().map(|l| l.len()).max().unwrap_or(0);

                // Store graph metadata for interactive edge tracing.
                // Save old task_order before overwriting — needed for
                // selection-by-ID preservation below.
                let old_task_order = std::mem::take(&mut self.task_order);
                self.node_line_map = viz_output.node_line_map;
                self.task_order = viz_output.task_order;
                self.forward_edges = viz_output.forward_edges;
                self.reverse_edges = viz_output.reverse_edges;
                self.char_edge_map = viz_output.char_edge_map;
                self.cycle_members = viz_output.cycle_members;

                // Preserve selection by task ID (not index) across refreshes.
                // The task_order may have changed, so resolve the old ID to
                // its new position.
                let prev_selected_id = self
                    .selected_task_idx
                    .and_then(|i| old_task_order.get(i))
                    .cloned();

                if let Some(ref prev_id) = prev_selected_id {
                    // Find the previously selected task in the new order.
                    self.selected_task_idx = self
                        .task_order
                        .iter()
                        .position(|id| id == prev_id)
                        .or_else(|| {
                            // Task disappeared — clamp to end.
                            if self.task_order.is_empty() {
                                None
                            } else {
                                Some(self.task_order.len() - 1)
                            }
                        });
                } else if !self.task_order.is_empty() {
                    // Default to first task on initial load (top of graph).
                    self.selected_task_idx = Some(0);
                }

                // Check for new-task focus marker (written by `wg add`).
                // If present, override selection to the newly created task.
                let new_task_focused = self.check_new_task_focus();

                self.recompute_trace();

                self.update_scroll_bounds();

                // Preserve viewport scroll position across graph refreshes
                // using a relative-position anchor: keep the selected task at
                // the same visual offset from the viewport top, even if lines
                // were inserted/removed above it.
                let new_selected_id = self.selected_task_id().map(String::from);
                let selection_unchanged = !new_task_focused
                    && old_selected_id.is_some()
                    && old_selected_id == new_selected_id;

                if self.initial_load {
                    // First load: scroll to top so tasks are visible immediately.
                    self.scroll.go_top();
                    self.initial_load = false;
                } else if was_at_bottom && !new_task_focused {
                    // Smart-follow: user was at the bottom, keep them there.
                    self.scroll.go_bottom();
                } else if selection_unchanged {
                    // Try to anchor using the task's relative position.
                    let anchored = old_relative_pos
                        .and_then(|rel_pos| {
                            let id = new_selected_id.as_ref()?;
                            let new_orig_line = *self.node_line_map.get(id)?;
                            let new_visible_pos = self.original_to_visible(new_orig_line)?;
                            // Compute new offset so the task stays at the same
                            // screen-relative position. Clamp to valid range.
                            let raw = new_visible_pos as isize - rel_pos;
                            let clamped = raw.max(0) as usize;
                            Some(clamped)
                        });
                    if let Some(new_offset) = anchored {
                        self.scroll.offset_y = new_offset;
                        self.scroll.clamp();
                    } else {
                        // Fallback: restore old offset and adjust if needed.
                        self.scroll.offset_y = old_offset_y;
                        self.scroll.clamp();
                        self.scroll_to_selected_task();
                    }
                } else {
                    // Selection changed (different task or first load) — center it.
                    self.scroll_to_selected_task();
                }
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
                self.char_edge_map.clear();
                self.cycle_members.clear();
                self.cycle_set.clear();
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
    /// Does NOT wrap around — stays at top when already at first task.
    pub fn select_prev_task(&mut self) {
        if self.task_order.is_empty() {
            return;
        }
        let idx = match self.selected_task_idx {
            Some(0) => return, // already at top, do nothing
            Some(i) => i - 1,
            None => 0,
        };
        self.selected_task_idx = Some(idx);
        self.recompute_trace();
        self.scroll_to_selected_task();
    }

    /// Move task selection to the next task in the viz order.
    /// Does NOT wrap around — stays at bottom when already at last task.
    pub fn select_next_task(&mut self) {
        if self.task_order.is_empty() {
            return;
        }
        let idx = match self.selected_task_idx {
            Some(i) if i + 1 >= self.task_order.len() => return, // already at bottom, do nothing
            Some(i) => i + 1,
            None => 0,
        };
        self.selected_task_idx = Some(idx);
        self.recompute_trace();
        self.scroll_to_selected_task();
    }

    /// Select the first task in the viz order.
    pub fn select_first_task(&mut self) {
        if self.task_order.is_empty() {
            return;
        }
        self.selected_task_idx = Some(0);
        self.recompute_trace();
        self.scroll_to_selected_task();
    }

    /// Select the last task in the viz order.
    pub fn select_last_task(&mut self) {
        if self.task_order.is_empty() {
            return;
        }
        self.selected_task_idx = Some(self.task_order.len() - 1);
        self.recompute_trace();
        self.scroll_to_selected_task();
    }

    /// Recompute the transitive upstream/downstream sets and line mappings
    /// based on the currently selected task.
    pub fn recompute_trace(&mut self) {
        self.upstream_set.clear();
        self.downstream_set.clear();
        self.cycle_set.clear();

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

        // Compute cycle membership for the selected task.
        if let Some(members) = self.cycle_members.get(&selected_id) {
            self.cycle_set = members.clone();
        }

        // Invalidate HUD and messages panel so they reload for the new selection.
        self.invalidate_hud();
        self.invalidate_log_pane();
        self.invalidate_messages_panel();
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
        if let Some(visible_pos) = self.original_to_visible(orig_line)
            && (visible_pos < self.scroll.offset_y
                || visible_pos >= self.scroll.offset_y + self.scroll.viewport_height)
        {
            let half = self.scroll.viewport_height / 2;
            self.scroll.offset_y = visible_pos.saturating_sub(half);
            self.scroll.clamp();
        }
    }

    /// Center the viewport on the selected task (unconditional — always recenters).
    pub fn center_on_selected_task(&mut self) {
        let task_id = match self.selected_task_idx.and_then(|i| self.task_order.get(i)) {
            Some(id) => id,
            None => return,
        };
        let orig_line = match self.node_line_map.get(task_id) {
            Some(&line) => line,
            None => return,
        };
        if let Some(visible_pos) = self.original_to_visible(orig_line) {
            let half = self.scroll.viewport_height / 2;
            self.scroll.offset_y = visible_pos.saturating_sub(half);
            self.scroll.clamp();
        }
    }

    /// Select the task at the given original line index, if any.
    /// Returns true if a task was found and selected.
    pub fn select_task_at_line(&mut self, orig_line: usize) -> bool {
        // Reverse lookup: find which task_id lives at this line.
        let task_id = self
            .node_line_map
            .iter()
            .find(|&(_, line)| *line == orig_line)
            .map(|(id, _)| id.clone());
        let task_id = match task_id {
            Some(id) => id,
            None => return false,
        };
        // Find its index in task_order.
        let idx = match self.task_order.iter().position(|id| *id == task_id) {
            Some(i) => i,
            None => return false,
        };
        self.selected_task_idx = Some(idx);
        self.recompute_trace();
        true
    }

    /// Get the currently selected task ID, if any.
    pub fn selected_task_id(&self) -> Option<&str> {
        self.selected_task_idx
            .and_then(|i| self.task_order.get(i))
            .map(|s| s.as_str())
    }

    /// Check for a new-task focus marker file and, if present, select that task.
    /// Returns true if selection was overridden to a newly created task.
    fn check_new_task_focus(&mut self) -> bool {
        let marker_path = self.workgraph_dir.join(".new_task_focus");
        let task_id = match std::fs::read_to_string(&marker_path) {
            Ok(id) => id.trim().to_string(),
            Err(_) => return false,
        };
        // Remove the marker immediately to avoid re-focusing on subsequent refreshes.
        let _ = std::fs::remove_file(&marker_path);

        if task_id.is_empty() {
            return false;
        }

        // Only auto-navigate to the new task when the user is on the Chat tab.
        // On other tabs (Detail, Log, Msg, Agency) the user is examining something
        // specific and shouldn't have their focus interrupted.
        if self.right_panel_tab != RightPanelTab::Chat {
            // Still show the notification so the user knows a task was added,
            // but don't move the selection or scroll.
            self.notification =
                Some((format!("New task: {}", task_id), Instant::now()));
            return false;
        }

        // Find the task in the current task_order.
        if let Some(idx) = self.task_order.iter().position(|id| id == &task_id) {
            self.selected_task_idx = Some(idx);
            // Set a transient highlight so the new task visually flashes.
            if let Some(&orig_line) = self.node_line_map.get(&task_id) {
                self.jump_target = Some((orig_line, Instant::now()));
            }
            self.notification =
                Some((format!("New task: {}", task_id), Instant::now()));
            true
        } else {
            false
        }
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
            if let Some((score, indices)) =
                self.matcher.fuzzy_indices(search_line, &self.search_input)
            {
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
                task.assigned
                    .as_ref()
                    .and_then(|aid| live_agent_usage.get(aid))
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
                // Update smart-follow state before reloading: track if user is at bottom.
                self.smart_follow_active = self.scroll.is_at_bottom();
                self.load_viz();
                if !self.search_input.is_empty() {
                    self.rerun_search();
                }
            }
            self.load_stats();
            self.load_agent_monitor();
            // Preserve HUD scroll position when the selected task hasn't changed.
            let prev_hud_task = self.hud_detail.as_ref().map(|d| d.task_id.clone());
            let prev_hud_scroll = self.hud_scroll;
            self.invalidate_hud();
            // Eagerly reload so we can restore scroll before render.
            self.load_hud_detail();
            if prev_hud_task.is_some()
                && prev_hud_task == self.hud_detail.as_ref().map(|d| d.task_id.clone())
            {
                self.hud_scroll = prev_hud_scroll;
            }
            // Reload log pane content if Log tab is active.
            if self.right_panel_tab == RightPanelTab::Log {
                self.invalidate_log_pane();
                self.load_log_pane();
            }
            // Reload messages panel if Messages tab is active.
            if self.right_panel_tab == RightPanelTab::Messages {
                self.invalidate_messages_panel();
                self.load_messages_panel();
            }
            self.last_refresh_display = chrono::Local::now().format("%H:%M:%S").to_string();
        }

        // Update coordinator status and poll for new chat messages on every refresh tick.
        if self.chat.awaiting_response || self.right_panel_tab == RightPanelTab::Chat {
            self.check_coordinator_status();
            self.poll_chat_messages();
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
                && let Some(task_usage) = self.task_token_map.get(&task_id)
            {
                usage.accumulate(task_usage);
            }
        }
        usage
    }

    /// Toggle mouse capture on/off.
    pub fn toggle_mouse(&mut self) {
        self.mouse_enabled = !self.mouse_enabled;
    }

    /// Toggle edge trace highlighting on/off.
    pub fn toggle_trace(&mut self) {
        self.trace_visible = !self.trace_visible;
        if !self.trace_visible {
            self.hud_detail = None;
            self.hud_scroll = 0;
        } else {
            self.load_hud_detail();
        }
    }

    /// Cycle to the next sort mode and re-sort the task_order.
    pub fn cycle_sort_mode(&mut self) {
        self.sort_mode = self.sort_mode.cycle();
        self.apply_sort_mode();
        // Adjust viewport based on sort mode.
        match self.sort_mode {
            SortMode::Chronological => {
                self.scroll.go_top();
                if !self.task_order.is_empty() {
                    self.selected_task_idx = Some(0);
                    self.recompute_trace();
                }
            }
            SortMode::ReverseChronological => {
                self.scroll.go_bottom();
                if !self.task_order.is_empty() {
                    self.selected_task_idx = Some(self.task_order.len() - 1);
                    self.recompute_trace();
                }
            }
            SortMode::StatusGrouped => {
                // Select the first task in priority order (likely in-progress).
                if !self.task_order.is_empty() {
                    self.selected_task_idx = Some(0);
                    self.recompute_trace();
                    self.scroll_to_selected_task();
                }
            }
        }
        self.notification = Some((
            format!("Sort: {}", self.sort_mode.label()),
            Instant::now(),
        ));
    }

    /// Apply the current sort mode to reorder `task_order`.
    /// Preserves the selected task ID across the reorder.
    fn apply_sort_mode(&mut self) {
        if self.task_order.is_empty() {
            return;
        }

        let prev_selected_id = self.selected_task_id().map(String::from);

        match self.sort_mode {
            SortMode::Chronological | SortMode::ReverseChronological => {
                // Sort by line number (original viz output order).
                // ReverseChronological uses the same order but starts the viewport at the bottom.
                self.task_order.sort_by_key(|id| {
                    self.node_line_map.get(id).copied().unwrap_or(usize::MAX)
                });
            }
            SortMode::StatusGrouped => {
                // Sort navigation order by status priority: in-progress first, then
                // failed, open, blocked, and done last. Within each group, preserve
                // the tree line order.
                let graph_path = self.workgraph_dir.join("graph.jsonl");
                let status_map: HashMap<String, u8> = match load_graph(&graph_path) {
                    Ok(g) => g
                        .tasks()
                        .map(|t| {
                            let priority = match t.status {
                                Status::InProgress => 0,
                                Status::Failed => 1,
                                Status::Open => 2,
                                Status::Blocked => 3,
                                Status::Done => 4,
                                Status::Abandoned => 5,
                            };
                            (t.id.clone(), priority)
                        })
                        .collect(),
                    Err(_) => HashMap::new(),
                };
                self.task_order.sort_by(|a, b| {
                    let sa = status_map.get(a).copied().unwrap_or(99);
                    let sb = status_map.get(b).copied().unwrap_or(99);
                    sa.cmp(&sb).then_with(|| {
                        let la = self.node_line_map.get(a).copied().unwrap_or(usize::MAX);
                        let lb = self.node_line_map.get(b).copied().unwrap_or(usize::MAX);
                        la.cmp(&lb)
                    })
                });
            }
        }

        // Restore selection by ID.
        if let Some(ref prev_id) = prev_selected_id {
            self.selected_task_idx = self
                .task_order
                .iter()
                .position(|id| id == prev_id)
                .or(Some(0));
        }
    }

    /// Load HUD detail for the currently selected task.
    /// Called when selection changes or trace is toggled on.
    pub fn load_hud_detail(&mut self) {
        let task_id = match self.selected_task_id() {
            Some(id) => id.to_string(),
            None => {
                self.hud_detail = None;
                return;
            }
        };

        // Skip reload if already loaded for this task.
        if let Some(ref detail) = self.hud_detail
            && detail.task_id == task_id
        {
            return;
        }

        self.hud_scroll = 0;

        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = match load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => {
                self.hud_detail = None;
                return;
            }
        };

        let task = match graph.tasks().find(|t| t.id == task_id) {
            Some(t) => t.clone(),
            None => {
                self.hud_detail = None;
                return;
            }
        };

        let mut lines: Vec<String> = Vec::new();

        // ── Header ──
        lines.push(format!("── {} ──", task.id));
        lines.push(format!("Title: {}", task.title));
        lines.push(format!("Status: {:?}", task.status));
        if let Some(ref agent) = task.assigned {
            lines.push(format!("Agent: {}", agent));
        }
        lines.push(String::new());

        // ── Description ──
        if let Some(ref desc) = task.description {
            lines.push("── Description ──".to_string());
            for (i, line) in desc.lines().enumerate() {
                if i >= 10 {
                    lines.push("  ...".to_string());
                    break;
                }
                lines.push(format!("  {}", line));
            }
            lines.push(String::new());
        }

        // ── Agent prompt (full) ──
        // Try live agent dir first, then fall back to archived logs
        let prompt_path = task
            .assigned
            .as_ref()
            .map(|aid| {
                self.workgraph_dir
                    .join("agents")
                    .join(aid)
                    .join("prompt.txt")
            })
            .filter(|p| p.exists())
            .or_else(|| find_latest_archive(&self.workgraph_dir, &task.id, "prompt.txt"));
        if let Some(prompt_path) = prompt_path {
            lines.push("── Prompt ──".to_string());
            if let Ok(file) = std::fs::File::open(&prompt_path) {
                let reader = BufReader::new(file);
                for line in reader.lines() {
                    if let Ok(l) = line {
                        lines.push(format!("  {}", l));
                    }
                }
            }
            lines.push(String::new());
        }

        // ── Agent output (full) ──
        // Try live agent dir first, then fall back to archived logs
        let output_path = task
            .assigned
            .as_ref()
            .map(|aid| {
                self.workgraph_dir
                    .join("agents")
                    .join(aid)
                    .join("output.log")
            })
            .filter(|p| p.exists())
            .or_else(|| find_latest_archive(&self.workgraph_dir, &task.id, "output.txt"));
        if let Some(output_path) = output_path {
            lines.push("── Output ──".to_string());
            if let Ok(content) = std::fs::read_to_string(&output_path) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if (trimmed.starts_with('{') || trimmed.starts_with('['))
                        && let Ok(val) =
                            serde_json::from_str::<serde_json::Value>(trimmed)
                    {
                        if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                            for pline in pretty.lines() {
                                lines.push(format!("  {}", pline));
                            }
                        } else {
                            lines.push(format!("  {}", line));
                        }
                    } else {
                        lines.push(format!("  {}", line));
                    }
                }
            }
            lines.push(String::new());
        }

        // ── Evaluation ──
        let evals_dir = self.workgraph_dir.join("agency").join("evaluations");
        if evals_dir.exists() {
            let prefix = format!("eval-{}-", task.id);
            if let Ok(entries) = std::fs::read_dir(&evals_dir) {
                let mut eval_found = false;
                // Find the most recent evaluation for this task.
                let mut eval_files: Vec<_> = entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                    .collect();
                eval_files.sort_by_key(|b| std::cmp::Reverse(b.file_name()));
                if let Some(entry) = eval_files.first()
                    && let Ok(content) = std::fs::read_to_string(entry.path())
                    && let Ok(eval) = serde_json::from_str::<serde_json::Value>(&content)
                {
                    eval_found = true;
                    lines.push("── Evaluation ──".to_string());
                    if let Some(score) = eval.get("score").and_then(|v| v.as_f64()) {
                        lines.push(format!("  Score: {:.2}", score));
                    }
                    if let Some(notes) = eval.get("notes").and_then(|v| v.as_str()) {
                        // Show first ~3 lines of notes.
                        for (i, line) in notes.lines().enumerate() {
                            if i >= 3 {
                                lines.push("  ...".to_string());
                                break;
                            }
                            lines.push(format!("  {}", line));
                        }
                    }
                    if let Some(dims) = eval.get("dimensions").and_then(|v| v.as_object()) {
                        let dim_strs: Vec<String> = dims
                            .iter()
                            .map(|(k, v)| format!("{}:{:.2}", k, v.as_f64().unwrap_or(0.0)))
                            .collect();
                        lines.push(format!("  Dims: {}", dim_strs.join(", ")));
                    }
                    lines.push(String::new());
                }
                let _ = eval_found;
            }
        }

        // ── Token usage ──
        if let Some(ref usage) = task.token_usage {
            lines.push("── Tokens ──".to_string());
            lines.push(format!(
                "  Input:  {} (→{})",
                format_tokens(usage.total_input()),
                format_tokens(usage.input_tokens)
            ));
            lines.push(format!(
                "  Output: {} (←{})",
                format_tokens(usage.output_tokens),
                format_tokens(usage.output_tokens)
            ));
            if usage.cache_read_input_tokens > 0 || usage.cache_creation_input_tokens > 0 {
                lines.push(format!(
                    "  Cache read:  {} (◎)",
                    format_tokens(usage.cache_read_input_tokens)
                ));
                lines.push(format!(
                    "  Cache write: {} (⊳)",
                    format_tokens(usage.cache_creation_input_tokens)
                ));
            }
            if usage.cost_usd > 0.0 {
                lines.push(format!("  Cost: ${:.4}", usage.cost_usd));
            }
            lines.push(String::new());
        }

        // ── Dependencies ──
        if !task.after.is_empty() || !task.before.is_empty() {
            lines.push("── Dependencies ──".to_string());
            if !task.after.is_empty() {
                lines.push(format!("  After:  {}", task.after.join(", ")));
            }
            if !task.before.is_empty() {
                lines.push(format!("  Before: {}", task.before.join(", ")));
            }
            lines.push(String::new());
        }

        // ── Timing ──
        let has_timing =
            task.created_at.is_some() || task.started_at.is_some() || task.completed_at.is_some();
        if has_timing {
            lines.push("── Timing ──".to_string());
            if let Some(ref ts) = task.created_at {
                lines.push(format!("  Created:   {}", format_timestamp(ts)));
            }
            if let Some(ref ts) = task.started_at {
                lines.push(format!("  Started:   {}", format_timestamp(ts)));
            }
            if let Some(ref ts) = task.completed_at {
                lines.push(format!("  Completed: {}", format_timestamp(ts)));
            }
            // Duration
            if let (Some(start), Some(end)) = (&task.started_at, &task.completed_at)
                && let (Ok(s), Ok(e)) = (
                    chrono::DateTime::parse_from_rfc3339(start),
                    chrono::DateTime::parse_from_rfc3339(end),
                )
            {
                let dur = (e - s).num_seconds();
                lines.push(format!(
                    "  Duration:  {}",
                    workgraph::format_duration(dur, false)
                ));
            }
            lines.push(String::new());
        }

        // ── Failure reason ──
        if let Some(ref reason) = task.failure_reason {
            lines.push("── Failure ──".to_string());
            lines.push(format!("  {}", reason));
            lines.push(String::new());
        }

        // Log entries are now shown in the dedicated log pane (L to toggle).

        self.hud_detail = Some(HudDetail {
            task_id,
            rendered_lines: lines,
        });
    }

    /// Invalidate HUD detail so it reloads on next render.
    pub fn invalidate_hud(&mut self) {
        self.hud_detail = None;
    }

    /// Scroll the HUD panel up.
    pub fn hud_scroll_up(&mut self, amount: usize) {
        self.hud_scroll = self.hud_scroll.saturating_sub(amount);
    }

    /// Scroll the HUD panel down using the cached wrapped line count and viewport height.
    pub fn hud_scroll_down(&mut self, amount: usize) {
        let max_scroll = self
            .hud_wrapped_line_count
            .saturating_sub(self.hud_detail_viewport_height);
        self.hud_scroll = (self.hud_scroll + amount).min(max_scroll);
    }

    // ── Log pane ──

    /// Load log entries for the currently selected task into the log pane.
    pub fn load_log_pane(&mut self) {
        let task_id = match self.selected_task_id() {
            Some(id) => id.to_string(),
            None => {
                self.log_pane.rendered_lines.clear();
                self.log_pane.task_id = None;
                return;
            }
        };

        // Skip reload if already loaded for this task and graph hasn't changed.
        if self.log_pane.task_id.as_deref() == Some(&task_id) {
            return;
        }

        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = match load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => {
                self.log_pane.rendered_lines.clear();
                self.log_pane.task_id = None;
                return;
            }
        };

        let task = match graph.tasks().find(|t| t.id == task_id) {
            Some(t) => t.clone(),
            None => {
                self.log_pane.rendered_lines.clear();
                self.log_pane.task_id = None;
                return;
            }
        };

        self.log_pane.rendered_lines.clear();

        if task.log.is_empty() {
            self.log_pane.rendered_lines.push("(no log entries)".to_string());
        } else {
            let now = chrono::Utc::now();
            for entry in &task.log {
                if self.log_pane.json_mode {
                    // Raw JSON format for debugging.
                    let json = serde_json::json!({
                        "timestamp": entry.timestamp,
                        "actor": entry.actor,
                        "message": entry.message,
                    });
                    self.log_pane.rendered_lines.push(json.to_string());
                } else {
                    // Human-readable format.
                    let time_str = format_relative_time(&entry.timestamp, &now);
                    self.log_pane.rendered_lines.push(format!(
                        "[{}] {}",
                        time_str, entry.message
                    ));
                }
            }
        }

        let new_count = self.log_pane.rendered_lines.len();

        // If auto-tail is on, scroll to bottom so newest entries are visible.
        if self.log_pane.auto_tail {
            let max_scroll = new_count.saturating_sub(self.log_pane.viewport_height);
            self.log_pane.scroll = max_scroll;
        }

        self.log_pane.task_id = Some(task_id);
    }

    /// Force reload of log pane content.
    pub fn invalidate_log_pane(&mut self) {
        self.log_pane.task_id = None;
    }

    /// Scroll log pane up.
    pub fn log_scroll_up(&mut self, amount: usize) {
        self.log_pane.scroll = self.log_pane.scroll.saturating_sub(amount);
        // User scrolled up — disable auto-tail.
        self.log_pane.auto_tail = false;
    }

    /// Scroll log pane down.
    pub fn log_scroll_down(&mut self, amount: usize) {
        let max_scroll = self
            .log_pane
            .rendered_lines
            .len()
            .saturating_sub(self.log_pane.viewport_height);
        self.log_pane.scroll = (self.log_pane.scroll + amount).min(max_scroll);
        // If we reached the bottom, resume auto-tail.
        if self.log_pane.scroll >= max_scroll {
            self.log_pane.auto_tail = true;
        }
    }

    /// Toggle log pane as right panel tab: switch to Log tab in right panel.
    pub fn toggle_log_pane(&mut self) {
        if self.right_panel_tab == RightPanelTab::Log && self.right_panel_visible {
            // Already showing log — toggle right panel off.
            self.right_panel_visible = false;
            self.focused_panel = FocusedPanel::Graph;
        } else {
            self.right_panel_visible = true;
            self.right_panel_tab = RightPanelTab::Log;
        }
    }

    /// Toggle log pane JSON mode.
    pub fn toggle_log_json(&mut self) {
        self.log_pane.json_mode = !self.log_pane.json_mode;
        self.invalidate_log_pane();
    }

    // ── Messages panel (panel 3) ──

    /// Load messages for the currently selected task into the messages panel.
    pub fn load_messages_panel(&mut self) {
        let task_id = match self.selected_task_id() {
            Some(id) => id.to_string(),
            None => {
                self.messages_panel.rendered_lines.clear();
                self.messages_panel.task_id = None;
                return;
            }
        };

        // Skip reload if already loaded for this task.
        if self.messages_panel.task_id.as_deref() == Some(&task_id) {
            return;
        }

        self.messages_panel.rendered_lines.clear();

        match workgraph::messages::list_messages(&self.workgraph_dir, &task_id) {
            Ok(msgs) if msgs.is_empty() => {
                self.messages_panel.rendered_lines.push("(no messages)".to_string());
            }
            Ok(msgs) => {
                let now = chrono::Utc::now();
                for msg in &msgs {
                    let time_str = format_relative_time(&msg.timestamp, &now);
                    let priority_tag = if msg.priority == "urgent" { " [!]" } else { "" };
                    self.messages_panel.rendered_lines.push(format!(
                        "[{}] {}{}: {}",
                        time_str, msg.sender, priority_tag, msg.body
                    ));
                }
            }
            Err(_) => {
                self.messages_panel.rendered_lines.push("(error loading messages)".to_string());
            }
        }

        self.messages_panel.task_id = Some(task_id);
    }

    /// Force reload of messages panel content.
    pub fn invalidate_messages_panel(&mut self) {
        self.messages_panel.task_id = None;
    }

    /// Construct a VizApp from pre-built VizOutput for unit testing.
    /// Avoids needing a real workgraph directory on disk.
    #[cfg(test)]
    pub(crate) fn from_viz_output_for_test(viz: &crate::commands::viz::VizOutput) -> Self {
        let lines: Vec<String> = viz.text.lines().map(String::from).collect();
        let plain_lines: Vec<String> = lines
            .iter()
            .map(|l| String::from_utf8(strip_ansi_escapes::strip(l.as_bytes())).unwrap_or_default())
            .collect();
        let search_lines = plain_lines.iter().map(|l| sanitize_for_search(l)).collect();
        let max_line_width = plain_lines.iter().map(|l| l.len()).max().unwrap_or(0);

        let mut task_order: Vec<(String, usize)> = viz
            .node_line_map
            .iter()
            .map(|(id, &line)| (id.clone(), line))
            .collect();
        task_order.sort_by_key(|(_, line)| *line);
        let task_order: Vec<String> = task_order.into_iter().map(|(id, _)| id).collect();

        let selected_task_idx = if task_order.is_empty() { None } else { Some(0) };

        Self {
            workgraph_dir: std::path::PathBuf::from("/tmp/test-workgraph"),
            viz_options: crate::commands::viz::VizOptions::default(),
            should_quit: false,
            lines,
            plain_lines,
            search_lines,
            max_line_width,
            scroll: ViewportScroll::new(),
            search_active: false,
            search_input: String::new(),
            fuzzy_matches: Vec::new(),
            current_match: None,
            filtered_indices: None,
            matcher: SkimMatcherV2::default(),
            task_counts: TaskCounts::default(),
            total_usage: workgraph::graph::TokenUsage {
                cost_usd: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            },
            task_token_map: HashMap::new(),
            show_total_tokens: false,
            show_help: false,
            mouse_enabled: false,
            last_graph_area: Rect::default(),
            last_right_panel_area: Rect::default(),
            last_tab_bar_area: Rect::default(),
            last_right_content_area: Rect::default(),
            jump_target: None,
            task_order,
            node_line_map: viz.node_line_map.clone(),
            forward_edges: viz.forward_edges.clone(),
            reverse_edges: viz.reverse_edges.clone(),
            selected_task_idx,
            trace_visible: true,
            upstream_set: HashSet::new(),
            downstream_set: HashSet::new(),
            char_edge_map: viz.char_edge_map.clone(),
            cycle_members: viz.cycle_members.clone(),
            cycle_set: HashSet::new(),
            hud_detail: None,
            hud_scroll: 0,
            hud_wrapped_line_count: 0,
            hud_detail_viewport_height: 0,
            right_panel_visible: false,
            focused_panel: FocusedPanel::Graph,
            right_panel_tab: RightPanelTab::Detail,
            right_panel_percent: 35,
            hud_size: HudSize::Normal,
            input_mode: InputMode::Normal,
            task_form: None,
            text_prompt: TextPromptState {
                input: String::new(),
            },
            chat: ChatState::default(),
            agent_monitor: AgentMonitorState::default(),
            log_pane: LogPaneState::default(),
            messages_panel: MessagesPanelState::default(),
            cmd_rx: mpsc::channel().1,
            cmd_tx: mpsc::channel().0,
            notification: None,
            last_tab_press: None,
            sort_mode: SortMode::ReverseChronological,
            smart_follow_active: true,
            initial_load: false,
            last_graph_mtime: None,
            last_refresh: Instant::now(),
            last_refresh_display: String::new(),
            refresh_interval: std::time::Duration::from_secs(3600),
        }
    }

    /// Force an immediate refresh (manual `r` key).
    pub fn force_refresh(&mut self) {
        self.last_graph_mtime = std::fs::metadata(self.workgraph_dir.join("graph.jsonl"))
            .and_then(|m| m.modified())
            .ok();
        self.smart_follow_active = self.scroll.is_at_bottom();
        self.load_viz();
        if !self.search_input.is_empty() {
            self.rerun_search();
        }
        self.load_stats();
        self.load_agent_monitor();
        self.last_refresh_display = chrono::Local::now().format("%H:%M:%S").to_string();
        self.last_refresh = Instant::now();
    }

    // ── Multi-panel methods ──

    /// Toggle focus between Graph and RightPanel.
    pub fn toggle_panel_focus(&mut self) {
        self.focused_panel = match self.focused_panel {
            FocusedPanel::Graph => {
                if self.right_panel_visible {
                    FocusedPanel::RightPanel
                } else {
                    FocusedPanel::Graph
                }
            }
            FocusedPanel::RightPanel => FocusedPanel::Graph,
        };
    }

    /// Toggle right panel visibility.
    pub fn toggle_right_panel(&mut self) {
        self.right_panel_visible = !self.right_panel_visible;
        if !self.right_panel_visible {
            self.focused_panel = FocusedPanel::Graph;
        }
    }

    /// Cycle HUD panel size between Normal (~1/3) and Expanded (~2/3).
    pub fn cycle_hud_size(&mut self) {
        self.hud_size = self.hud_size.cycle();
        self.right_panel_percent = self.hud_size.side_percent();
    }

    /// Execute a wg command in a background thread.
    pub fn exec_command(&self, args: Vec<String>, effect: CommandEffect) {
        let tx = self.cmd_tx.clone();
        // self.workgraph_dir is the `.workgraph` directory itself (e.g.
        // /project/.workgraph). The `wg` binary expects to run from the
        // project root so it can find `.workgraph` as a child — running
        // from *inside* `.workgraph` causes it to look for the non-existent
        // `.workgraph/.workgraph`. Use the parent directory as the CWD.
        let project_root = self
            .workgraph_dir
            .parent()
            .unwrap_or(&self.workgraph_dir)
            .to_path_buf();
        std::thread::spawn(move || {
            let result = Command::new("wg")
                .args(&args)
                .current_dir(&project_root)
                .output();
            let (success, output) = match result {
                Ok(o) => {
                    let stdout = String::from_utf8_lossy(&o.stdout).to_string();
                    let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                    let combined = if stderr.is_empty() {
                        stdout
                    } else {
                        format!("{}\n{}", stdout, stderr)
                    };
                    (o.status.success(), combined)
                }
                Err(e) => (false, format!("Failed to run wg: {}", e)),
            };
            let _ = tx.send(CommandResult {
                success,
                output,
                effect,
            });
        });
    }

    /// Drain any completed background commands and apply their effects.
    pub fn drain_commands(&mut self) {
        while let Ok(result) = self.cmd_rx.try_recv() {
            match result.effect {
                CommandEffect::Refresh => {
                    self.force_refresh();
                }
                CommandEffect::Notify(msg) => {
                    let msg = if result.success {
                        msg
                    } else {
                        let err = result.output.lines().find(|l| !l.is_empty()).unwrap_or("unknown");
                        format!("Error: {}", err)
                    };
                    self.notification = Some((msg, Instant::now()));
                }
                CommandEffect::RefreshAndNotify(msg) => {
                    self.force_refresh();
                    let msg = if result.success {
                        msg
                    } else {
                        let err = result.output.lines().find(|l| !l.is_empty()).unwrap_or("unknown");
                        format!("Error: {}", err)
                    };
                    self.notification = Some((msg, Instant::now()));
                }
                CommandEffect::ChatResponse(request_id) => {
                    // On failure, show error (coordinator didn't write to outbox).
                    if !result.success {
                        // Use first non-empty line: when stdout is empty the
                        // combined output starts with "\n{stderr}", so
                        // lines().next() would be an empty string.
                        let error_line = result
                            .output
                            .lines()
                            .find(|l| !l.is_empty())
                            .unwrap_or("send failed");
                        self.chat.messages.push(ChatMessage {
                            role: ChatRole::System,
                            text: format!("Error: {}", error_line),
                        });
                    }
                    // Clear awaiting state — the response arrives via poll_chat_messages.
                    // Don't push the response here to avoid duplicates with poll.
                    if self.chat.last_request_id.as_deref() == Some(&request_id) {
                        self.chat.awaiting_response = false;
                        self.chat.last_request_id = None;
                    }
                    // Auto-scroll to bottom.
                    self.chat.scroll = 0;
                    // Refresh graph in case coordinator created tasks.
                    self.force_refresh();
                }
            }
        }
        // Clear expired notifications (after 3 seconds).
        if let Some((_, when)) = &self.notification {
            if when.elapsed() > std::time::Duration::from_secs(3) {
                self.notification = None;
            }
        }
    }

    /// Load agent monitor data from the agent registry.
    pub fn load_agent_monitor(&mut self) {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = load_graph(&graph_path).ok();

        match AgentRegistry::load(&self.workgraph_dir) {
            Ok(registry) => {
                self.agent_monitor.agents = registry
                    .agents
                    .iter()
                    .map(|(id, agent)| {
                        let tid = &agent.task_id;
                        let task_title = graph
                            .as_ref()
                            .and_then(|g| g.tasks().find(|t| t.id == *tid))
                            .map(|t| t.title.clone());
                        let started =
                            chrono::DateTime::parse_from_rfc3339(&agent.started_at)
                                .ok()
                                .map(|dt| dt.with_timezone(&chrono::Utc));
                        let completed = agent
                            .completed_at
                            .as_deref()
                            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                            .map(|dt| dt.with_timezone(&chrono::Utc));
                        // For alive agents: elapsed since start. For finished: start→end.
                        let runtime_secs = started.map(|s| {
                            let end = completed.unwrap_or_else(chrono::Utc::now);
                            (end - s).num_seconds()
                        });
                        AgentMonitorEntry {
                            agent_id: id.clone(),
                            task_id: Some(agent.task_id.clone()),
                            task_title,
                            status: agent.status,
                            runtime_secs,
                            started_at: Some(agent.started_at.clone()),
                            completed_at: agent.completed_at.clone(),
                        }
                    })
                    .collect();
                // Sort: working agents first, then by ID.
                self.agent_monitor.agents.sort_by(|a, b| {
                    let a_working = matches!(a.status, AgentStatus::Working);
                    let b_working = matches!(b.status, AgentStatus::Working);
                    b_working.cmp(&a_working).then(a.agent_id.cmp(&b.agent_id))
                });
            }
            Err(_) => {
                self.agent_monitor.agents.clear();
            }
        }
    }

    // ── Chat methods ──

    /// Check whether the service daemon is running (coordinator active).
    pub fn check_coordinator_status(&mut self) {
        use crate::commands::service::{ServiceState, is_service_alive};
        self.chat.coordinator_active = ServiceState::load(&self.workgraph_dir)
            .ok()
            .flatten()
            .is_some_and(|s| is_service_alive(s.pid));
    }

    /// Load chat history from inbox/outbox files on startup.
    pub fn load_chat_history(&mut self) {
        let history = match workgraph::chat::read_history(&self.workgraph_dir) {
            Ok(h) => h,
            Err(_) => return,
        };

        self.chat.messages.clear();
        for msg in &history {
            let role = match msg.role.as_str() {
                "user" => ChatRole::User,
                "coordinator" => ChatRole::Coordinator,
                _ => ChatRole::System,
            };
            self.chat.messages.push(ChatMessage {
                role,
                text: msg.content.clone(),
            });
        }

        // Set outbox cursor to latest outbox message ID so we don't re-display old messages.
        if let Ok(msgs) = workgraph::chat::read_outbox_since(&self.workgraph_dir, 0) {
            self.chat.outbox_cursor = msgs.last().map(|m| m.id).unwrap_or(0);
        }
        // Also track inbox cursor to detect messages from other sources.
        // (We just loaded history, so we're caught up.)
    }

    /// Poll for new coordinator responses in the outbox.
    /// Called during refresh ticks.
    pub fn poll_chat_messages(&mut self) {
        let new_msgs =
            match workgraph::chat::read_outbox_since(&self.workgraph_dir, self.chat.outbox_cursor)
            {
                Ok(msgs) => msgs,
                Err(_) => return,
            };

        if new_msgs.is_empty() {
            return;
        }

        for msg in &new_msgs {
            self.chat.messages.push(ChatMessage {
                role: ChatRole::Coordinator,
                text: msg.content.clone(),
            });
        }

        // Update cursor to latest message.
        self.chat.outbox_cursor = new_msgs.last().map(|m| m.id).unwrap_or(self.chat.outbox_cursor);

        // Any new coordinator response clears the awaiting state.
        // The TUI request_id ("tui-...") differs from wg chat's ("chat-..."),
        // so we clear on any new outbox message rather than matching by ID.
        if self.chat.awaiting_response {
            self.chat.awaiting_response = false;
            self.chat.last_request_id = None;
        }

        // Auto-scroll to bottom when new messages arrive (if user hasn't scrolled up).
        if self.chat.scroll == 0 {
            // Already at bottom; new messages will be visible.
        }
    }

    /// Send a chat message to the coordinator via IPC.
    /// Appends the user message to display immediately and starts background send.
    pub fn send_chat_message(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }

        // Generate a request ID for correlating the response.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let request_id = format!(
            "tui-{}-{}",
            now.as_millis(),
            now.subsec_nanos() % 100_000
        );

        // Add user message to display immediately.
        self.chat.messages.push(ChatMessage {
            role: ChatRole::User,
            text: text.clone(),
        });

        // Reset scroll to bottom.
        self.chat.scroll = 0;

        // Mark as awaiting response.
        self.chat.awaiting_response = true;
        self.chat.last_request_id = Some(request_id.clone());

        // Send via `wg chat` command in background.
        // The command writes to inbox and sends IPC UserChat to daemon.
        self.exec_command(
            vec!["chat".to_string(), text],
            CommandEffect::ChatResponse(request_id),
        );
    }

    /// Open the task creation form.
    pub fn open_task_form(&mut self) {
        self.task_form = Some(TaskFormState::new(&self.workgraph_dir));
        self.input_mode = InputMode::TaskForm;
    }

    /// Close the task creation form.
    pub fn close_task_form(&mut self) {
        self.task_form = None;
        self.input_mode = InputMode::Normal;
    }

    /// Submit the task creation form — runs `wg add` in background.
    pub fn submit_task_form(&mut self) {
        let form = match self.task_form.take() {
            Some(f) => f,
            None => return,
        };
        self.input_mode = InputMode::Normal;

        if form.title.trim().is_empty() {
            self.notification = Some(("Task title is required".to_string(), Instant::now()));
            return;
        }

        let mut args = vec!["add".to_string(), form.title.trim().to_string()];

        if !form.description.trim().is_empty() {
            args.push("-d".to_string());
            args.push(form.description.trim().to_string());
        }

        if !form.selected_deps.is_empty() {
            args.push("--after".to_string());
            args.push(form.selected_deps.join(","));
        }

        if !form.tags.trim().is_empty() {
            for tag in form.tags.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()) {
                args.push("--tag".to_string());
                args.push(tag.to_string());
            }
        }

        self.exec_command(args, CommandEffect::RefreshAndNotify("Task created".to_string()));
    }

    /// Kill the agent assigned to the currently selected task.
    ///
    /// Loads the graph to find the task's `assigned` field, then runs
    /// `wg kill <agent-id>` in the background. Shows a notification on
    /// success or if no agent is active.
    pub fn kill_focused_agent(&mut self) {
        let task_id = match self.selected_task_id() {
            Some(id) => id.to_string(),
            None => {
                self.notification =
                    Some(("No task selected".to_string(), Instant::now()));
                return;
            }
        };

        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = match load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => {
                self.notification =
                    Some(("Failed to load graph".to_string(), Instant::now()));
                return;
            }
        };

        let agent_id = match graph.tasks().find(|t| t.id == task_id) {
            Some(task) => match &task.assigned {
                Some(id) => id.clone(),
                None => {
                    self.notification = Some((
                        format!("No active agent on '{}'", task_id),
                        Instant::now(),
                    ));
                    return;
                }
            },
            None => {
                self.notification =
                    Some((format!("Task '{}' not found", task_id), Instant::now()));
                return;
            }
        };

        self.exec_command(
            vec!["kill".to_string(), agent_id.clone()],
            CommandEffect::RefreshAndNotify(format!(
                "Killed {} on task '{}'",
                agent_id, task_id
            )),
        );
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
    let parts: Vec<&str> = stdout.split_whitespace().collect();
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
                    && indent < need_below
                {
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
    let after_connectors: &str =
        trimmed.trim_start_matches(|c: char| is_box_drawing(c) || c == ' ');
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

/// Find the most recent archived agent file for a task.
///
/// Looks in `.workgraph/log/agents/<task-id>/` for timestamped subdirectories
/// and returns the path to `filename` in the most recent one (if it exists).
fn find_latest_archive(
    workgraph_dir: &std::path::Path,
    task_id: &str,
    filename: &str,
) -> Option<std::path::PathBuf> {
    let archive_base = workgraph_dir.join("log").join("agents").join(task_id);
    if !archive_base.exists() {
        return None;
    }
    let mut entries: Vec<_> = std::fs::read_dir(&archive_base)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().ok().is_some_and(|ft| ft.is_dir()))
        .collect();
    // Sort by name descending (timestamps sort lexicographically)
    entries.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    for entry in entries {
        let candidate = entry.path().join(filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Format an ISO 8601 timestamp for HUD display (shorter, local time).
fn format_timestamp(ts: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => {
            let local = dt.with_timezone(&chrono::Local);
            local.format("%Y-%m-%d %H:%M:%S").to_string()
        }
        Err(_) => ts.to_string(),
    }
}

/// Format an ISO 8601 timestamp as a relative time string (e.g. "5m ago", "2h ago").
/// Falls back to short datetime if parsing fails.
fn format_relative_time(ts: &str, now: &chrono::DateTime<chrono::Utc>) -> String {
    let dt = match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => dt.with_timezone(&chrono::Utc),
        Err(_) => return ts.to_string(),
    };
    let delta = *now - dt;
    let secs = delta.num_seconds();
    if secs < 0 {
        // Future timestamp — just show the time.
        return dt
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();
    }
    if secs < 60 {
        format!("{}s ago", secs)
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
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

    /// Returns true if the viewport is scrolled to (or near) the bottom.
    /// "Near" means within 3 lines of the bottom, to allow for small render jitter.
    pub fn is_at_bottom(&self) -> bool {
        let max_y = self.content_height.saturating_sub(self.viewport_height);
        self.offset_y + 3 >= max_y || self.content_height <= self.viewport_height
    }

    /// Clamp scroll offset to valid range after content changes.
    pub fn clamp(&mut self) {
        let max_y = self.content_height.saturating_sub(self.viewport_height);
        self.offset_y = self.offset_y.min(max_y);
        let max_x = self.content_width.saturating_sub(self.viewport_width);
        self.offset_x = self.offset_x.min(max_x);
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests for HUD state and behavior
// ══════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod hud_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use workgraph::graph::{Node, Status, TokenUsage, WorkGraph};
    use workgraph::parser::save_graph;
    use workgraph::test_helpers::make_task_with_status;

    use crate::commands::viz::ascii::generate_ascii;
    use crate::commands::viz::{LayoutMode, VizOutput};

    /// Build a chain graph a -> b -> c plus standalone d, with rich metadata on task a.
    /// Returns (VizOutput, WorkGraph, TempDir) — keep TempDir alive while using the app.
    fn build_chain_plus_isolated() -> (VizOutput, WorkGraph, tempfile::TempDir) {
        let mut graph = WorkGraph::new();
        let mut a = make_task_with_status("a", "Task Alpha", Status::Done);
        a.description = Some(
            "This is the description for task Alpha.\nLine two.\nLine three.\nLine four."
                .to_string(),
        );
        a.assigned = Some("agent-001".to_string());
        a.created_at = Some("2026-01-15T10:00:00Z".to_string());
        a.started_at = Some("2026-01-15T10:05:00Z".to_string());
        a.completed_at = Some("2026-01-15T10:30:00Z".to_string());
        a.token_usage = Some(TokenUsage {
            cost_usd: 0.05,
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_input_tokens: 200,
            cache_creation_input_tokens: 100,
        });

        let mut b = make_task_with_status("b", "Task Bravo", Status::InProgress);
        b.after = vec!["a".to_string()];
        b.assigned = Some("agent-002".to_string());
        b.description = Some("Description for Bravo.".to_string());

        let mut c = make_task_with_status("c", "Task Charlie", Status::Open);
        c.after = vec!["b".to_string()];
        // No description, no agent, no tokens — for missing-data tests.

        let mut d = make_task_with_status("d", "Task Delta", Status::Failed);
        d.failure_reason = Some("Timed out after 30 minutes".to_string());
        d.description = Some("Delta task description.".to_string());

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        // Create a temp directory with graph.jsonl so load_hud_detail works.
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph.jsonl");
        save_graph(&graph, &graph_path).unwrap();

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
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
        );
        (result, graph, tmp)
    }

    /// Build a VizApp with a specific task selected, pointed at a real workgraph dir.
    fn build_app(viz: &VizOutput, selected_id: &str, workgraph_dir: &std::path::Path) -> VizApp {
        let mut app = VizApp::from_viz_output_for_test(viz);
        app.workgraph_dir = workgraph_dir.to_path_buf();
        let idx = app.task_order.iter().position(|id| id == selected_id);
        app.selected_task_idx = idx;
        app.recompute_trace();
        app
    }

    // ── TEST 1: HUD APPEARS WITH TAB ──

    #[test]
    fn hud_visible_when_trace_on_and_task_selected() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let app = build_app(&viz, "a", _tmp.path());

        assert!(app.trace_visible, "trace_visible should default to true");
        assert!(
            app.selected_task_idx.is_some(),
            "should have a selected task"
        );
        let show_hud = app.trace_visible && app.selected_task_idx.is_some();
        assert!(
            show_hud,
            "HUD should be visible when trace is on and task is selected"
        );
    }

    // ── TEST 2: HUD DISAPPEARS WITH TAB ──

    #[test]
    fn hud_hidden_when_trace_toggled_off() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());

        app.toggle_trace();
        assert!(!app.trace_visible, "trace should be off after toggle");
        assert!(
            app.hud_detail.is_none(),
            "HUD detail should be cleared when trace is off"
        );
        assert_eq!(app.hud_scroll, 0, "HUD scroll should reset");

        let show_hud = app.trace_visible && app.selected_task_idx.is_some();
        assert!(!show_hud, "HUD should NOT be visible when trace is off");
    }

    #[test]
    fn hud_reappears_after_double_toggle() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());

        app.toggle_trace(); // off
        app.toggle_trace(); // on
        assert!(app.trace_visible);
        let show_hud = app.trace_visible && app.selected_task_idx.is_some();
        assert!(show_hud, "HUD should reappear after toggling back on");
    }

    // ── TEST 3: HUD CONTENT CORRECT ──

    #[test]
    fn hud_shows_task_id_and_title() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().expect("HUD detail should load");
        assert_eq!(detail.task_id, "a");
        assert!(detail.rendered_lines.iter().any(|l| l.contains("── a ──")));
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Title: Task Alpha"))
        );
    }

    #[test]
    fn hud_shows_status() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Status: Done"))
        );
    }

    #[test]
    fn hud_shows_agent() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Agent: agent-001"))
        );
    }

    #[test]
    fn hud_shows_description_excerpt() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Description ──"))
        );
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("This is the description for task Alpha."))
        );
    }

    #[test]
    fn hud_shows_token_usage() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Tokens ──"))
        );
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Cost: $0.05"))
        );
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Cache read:"))
        );
    }

    #[test]
    fn hud_shows_dependencies() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "b", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Dependencies ──"))
        );
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("After:") && l.contains("a"))
        );
    }

    #[test]
    fn hud_shows_timing() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Timing ──"))
        );
        assert!(detail.rendered_lines.iter().any(|l| l.contains("Created:")));
        assert!(detail.rendered_lines.iter().any(|l| l.contains("Started:")));
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Completed:"))
        );
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Duration:"))
        );
    }

    #[test]
    fn hud_shows_failure_reason() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "d", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Failure ──"))
        );
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Timed out after 30 minutes"))
        );
    }

    // ── TEST 4: HUD UPDATES ON SELECTION ──

    #[test]
    fn hud_invalidates_on_selection_change() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();
        assert_eq!(app.hud_detail.as_ref().unwrap().task_id, "a");

        app.select_next_task();
        // recompute_trace calls invalidate_hud
        assert!(
            app.hud_detail.is_none(),
            "HUD should be invalidated after selection change"
        );

        app.load_hud_detail();
        let new_id = app.hud_detail.as_ref().unwrap().task_id.clone();
        assert_ne!(new_id, "a", "HUD should now show a different task");
    }

    #[test]
    fn hud_content_changes_on_navigation() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();
        let initial = app.hud_detail.as_ref().unwrap().rendered_lines.clone();

        app.select_next_task();
        app.load_hud_detail();
        let next = app.hud_detail.as_ref().unwrap().rendered_lines.clone();

        assert_ne!(
            initial, next,
            "HUD content should change when selecting a different task"
        );
    }

    #[test]
    fn hud_updates_on_prev_task() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());

        app.select_next_task();
        app.load_hud_detail();
        let second_id = app.hud_detail.as_ref().unwrap().task_id.clone();

        app.select_prev_task();
        app.load_hud_detail();
        let back_id = app.hud_detail.as_ref().unwrap().task_id.clone();

        assert_ne!(
            second_id, back_id,
            "HUD should show different content after navigating back"
        );
    }

    // ── TEST 5: NARROW TERMINAL FALLBACK ──
    // (Layout tests are in render.rs test module below)

    // ── TEST 6: HUD SCROLLABLE ──

    #[test]
    fn hud_scroll_down() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let total = app.hud_detail.as_ref().unwrap().rendered_lines.len();
        assert!(total > 5, "precondition: need >5 lines to test scrolling");

        // Simulate renderer setting wrapped line count and viewport.
        app.hud_wrapped_line_count = total;
        app.hud_detail_viewport_height = 10;

        assert_eq!(app.hud_scroll, 0);
        app.hud_scroll_down(3);
        assert_eq!(app.hud_scroll, 3);
    }

    #[test]
    fn hud_scroll_up() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let total = app.hud_detail.as_ref().unwrap().rendered_lines.len();
        app.hud_wrapped_line_count = total;
        app.hud_detail_viewport_height = 10;
        app.hud_scroll_down(5);
        assert_eq!(app.hud_scroll, 5);

        app.hud_scroll_up(2);
        assert_eq!(app.hud_scroll, 3);
    }

    #[test]
    fn hud_scroll_clamps_at_zero() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        app.hud_scroll_up(10);
        assert_eq!(app.hud_scroll, 0, "scroll should not go below 0");
    }

    #[test]
    fn hud_scroll_clamps_at_max() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let total = app.hud_detail.as_ref().unwrap().rendered_lines.len();
        let viewport = 10;
        let max_scroll = total.saturating_sub(viewport);

        app.hud_wrapped_line_count = total;
        app.hud_detail_viewport_height = viewport;
        app.hud_scroll_down(1000);
        assert_eq!(app.hud_scroll, max_scroll, "scroll should clamp at max");
    }

    #[test]
    fn hud_scroll_resets_on_selection_change() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let total = app.hud_detail.as_ref().unwrap().rendered_lines.len();
        app.hud_wrapped_line_count = total;
        app.hud_detail_viewport_height = 10;
        app.hud_scroll_down(5);
        assert!(app.hud_scroll > 0);

        app.select_next_task();
        app.load_hud_detail();
        assert_eq!(app.hud_scroll, 0, "scroll should reset for new task");
    }

    // ── TEST 7: NO CRASH ON MISSING DATA ──

    #[test]
    fn hud_no_crash_no_agent() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "c", _tmp.path());
        app.load_hud_detail();

        let detail = app
            .hud_detail
            .as_ref()
            .expect("should load even with no agent");
        assert_eq!(detail.task_id, "c");
        assert!(
            !detail
                .rendered_lines
                .iter()
                .any(|l| l.starts_with("Agent:"))
        );
    }

    #[test]
    fn hud_no_crash_no_description() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "c", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            !detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Description ──"))
        );
    }

    #[test]
    fn hud_no_crash_no_tokens() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "c", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            !detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Tokens ──"))
        );
    }

    #[test]
    fn hud_no_crash_no_timing() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "c", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            !detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Timing ──"))
        );
    }

    #[test]
    fn hud_no_crash_no_failure() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            !detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Failure ──"))
        );
    }

    #[test]
    fn hud_no_crash_no_dependencies() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "d", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            !detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Dependencies ──"))
        );
    }

    #[test]
    fn hud_no_crash_no_selection() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.workgraph_dir = _tmp.path().to_path_buf();
        app.selected_task_idx = None;

        app.load_hud_detail();
        assert!(app.hud_detail.is_none());
    }

    #[test]
    fn hud_no_crash_empty_graph() {
        let empty_viz = crate::commands::viz::VizOutput {
            text: "(no tasks to display)".to_string(),
            node_line_map: HashMap::new(),
            task_order: Vec::new(),
            forward_edges: HashMap::new(),
            reverse_edges: HashMap::new(),
            char_edge_map: HashMap::new(),
            cycle_members: HashMap::new(),
        };

        let mut app = VizApp::from_viz_output_for_test(&empty_viz);
        assert!(app.selected_task_idx.is_none());

        app.load_hud_detail();
        assert!(app.hud_detail.is_none());

        // Toggle trace on empty graph should not panic
        app.toggle_trace();
        assert!(!app.trace_visible);
        app.toggle_trace();
        assert!(app.trace_visible);
    }

    // ── ADDITIONAL: skip-reload optimization ──

    #[test]
    fn hud_skips_reload_for_same_task() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();
        assert_eq!(app.hud_detail.as_ref().unwrap().task_id, "a");

        // Second load should be a no-op
        app.load_hud_detail();
        assert_eq!(app.hud_detail.as_ref().unwrap().task_id, "a");
    }

    #[test]
    fn hud_invalidate_forces_reload() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();
        assert!(app.hud_detail.is_some());

        app.invalidate_hud();
        assert!(app.hud_detail.is_none());

        app.load_hud_detail();
        assert_eq!(app.hud_detail.as_ref().unwrap().task_id, "a");
    }

    // ── ADDITIONAL: description truncation ──

    #[test]
    fn hud_description_truncated_to_10_lines() {
        let mut graph = WorkGraph::new();
        let mut task = make_task_with_status("long-desc", "Long Description Task", Status::Open);
        task.description = Some(
            (0..15)
                .map(|i| format!("Line {}", i))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        graph.add_node(Node::Task(task));

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
            &HashMap::new(),
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
        );

        let mut app = build_app(&viz, "long-desc", tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        let desc_start = detail
            .rendered_lines
            .iter()
            .position(|l| l.contains("── Description ──"))
            .expect("should have description section");

        let desc_lines: Vec<_> = detail.rendered_lines[desc_start + 1..]
            .iter()
            .take_while(|l| !l.is_empty())
            .collect();

        // Should have at most 11 lines (10 content + 1 "  ..." truncation indicator)
        assert!(
            desc_lines.len() <= 11,
            "Description should be truncated, got {} lines",
            desc_lines.len()
        );
        assert!(
            desc_lines.iter().any(|l| l.contains("...")),
            "Truncated description should show '...' indicator"
        );
    }
}
