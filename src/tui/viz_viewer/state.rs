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
use workgraph::config::Config;
use workgraph::graph::{Status, TokenUsage, format_tokens, parse_token_usage_live};
use workgraph::models::load_model_choices;
use workgraph::parser::load_graph;
use workgraph::{AgentRegistry, AgentStatus};

use edtui::{EditorEventHandler, EditorMode, EditorState};

pub fn new_emacs_editor() -> EditorState {
    let mut state = EditorState::default();
    state.mode = EditorMode::Insert;
    state
}

#[allow(dead_code)]
pub fn new_emacs_editor_with(text: &str) -> EditorState {
    use edtui::Lines;
    let mut state = EditorState::new(Lines::from(text));
    state.mode = EditorMode::Insert;
    state
}

pub fn editor_text(state: &EditorState) -> String {
    state.lines.to_string()
}

pub fn editor_is_empty(state: &EditorState) -> bool {
    state.lines.to_string().is_empty()
}

pub fn editor_clear(state: &mut EditorState) {
    *state = new_emacs_editor();
}

pub fn create_editor_handler() -> EditorEventHandler {
    use edtui::actions::delete::DeleteToEndOfLine;
    use edtui::actions::{
        MoveBackward, MoveDown, MoveForward, MoveToEndOfLine, MoveToStartOfLine, MoveUp,
    };
    use edtui::events::{KeyEvent as EdKeyEvent, KeyEventRegister};
    let mut handler = EditorEventHandler::default();
    // Emacs keybindings for insert mode
    handler.key_handler.insert(
        KeyEventRegister::i(vec![EdKeyEvent::Ctrl('a')]),
        MoveToStartOfLine(),
    );
    handler.key_handler.insert(
        KeyEventRegister::i(vec![EdKeyEvent::Ctrl('e')]),
        MoveToEndOfLine(),
    );
    handler.key_handler.insert(
        KeyEventRegister::i(vec![EdKeyEvent::Ctrl('f')]),
        MoveForward(1),
    );
    handler.key_handler.insert(
        KeyEventRegister::i(vec![EdKeyEvent::Ctrl('b')]),
        MoveBackward(1),
    );
    handler.key_handler.insert(
        KeyEventRegister::i(vec![EdKeyEvent::Ctrl('k')]),
        DeleteToEndOfLine,
    );
    handler.key_handler.insert(
        KeyEventRegister::i(vec![EdKeyEvent::Ctrl('n')]),
        MoveDown(1),
    );
    handler.key_handler.insert(
        KeyEventRegister::i(vec![EdKeyEvent::Ctrl('p')]),
        MoveUp(1),
    );
    // Ctrl-U (kill to beginning of line) is already mapped by edtui default
    handler
}

/// Maximum simultaneous animations before oldest are dropped.
const MAX_ANIMATIONS: usize = 50;

/// Token-count bucket size for debouncing token-usage animations.
/// Changes smaller than this are ignored.
const TOKEN_DEBOUNCE_BUCKET: u64 = 1000;

// ══════════════════════════════════════════════════════════════════════════════
// Animation system
// ══════════════════════════════════════════════════════════════════════════════

/// Animation speed preset, controlling the fade duration.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AnimationSpeed {
    Fast,
    Normal,
    Slow,
}

impl AnimationSpeed {
    /// Duration of the splash-and-fade animation in seconds.
    pub fn duration_secs(self) -> f64 {
        match self {
            Self::Fast => 0.4,
            Self::Normal => 0.8,
            Self::Slow => 1.5,
        }
    }
}

/// What kind of change triggered this animation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AnimationKind {
    /// A brand-new task appeared in the graph.
    NewTask,
    /// Task status changed (e.g. open → in-progress → done → failed).
    StatusChange,
    /// Non-status text changed (token counts, timestamps, etc.).
    ContentChange,
    /// An agent was assigned or changed on the task.
    Assignment,
    /// A new dependency edge appeared on the task.
    EdgeChange,
    /// A previously hidden task was revealed (e.g. toggling system task visibility).
    Revealed,
    /// A lifecycle phase transition: parent entered evaluation phase.
    Evaluation,
    /// A lifecycle phase transition: parent entered verification phase.
    Verification,
}

/// A single active flash-and-fade animation on a task.
#[derive(Clone)]
pub struct Animation {
    /// When the animation started.
    pub start: Instant,
    /// The flash color (at full brightness).
    pub flash_color: (u8, u8, u8),
    /// What triggered this animation.
    #[allow(dead_code)]
    pub kind: AnimationKind,
}

/// Animation mode from config.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AnimationMode {
    /// Normal animated flash-and-fade.
    Normal,
    /// Fast animation.
    Fast,
    /// Slow animation.
    Slow,
    /// Reduced motion: instant color change, no fade.
    Reduced,
    /// Animations disabled entirely.
    Off,
}

impl AnimationMode {
    pub fn from_config(s: &str) -> Self {
        match s {
            "fast" => Self::Fast,
            "slow" => Self::Slow,
            "reduced" => Self::Reduced,
            "off" => Self::Off,
            _ => Self::Normal,
        }
    }

    pub fn speed(self) -> AnimationSpeed {
        match self {
            Self::Fast => AnimationSpeed::Fast,
            Self::Normal | Self::Reduced => AnimationSpeed::Normal,
            Self::Slow => AnimationSpeed::Slow,
            Self::Off => AnimationSpeed::Normal, // unused when off
        }
    }

    pub fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Fast => "fast",
            Self::Slow => "slow",
            Self::Reduced => "reduced",
            Self::Off => "off",
        }
    }
}

/// Flash color for a given status transition.
fn flash_color_for_status(status: &Status) -> (u8, u8, u8) {
    match status {
        Status::Done => (80, 220, 100),       // green
        Status::Failed => (220, 60, 60),      // red
        Status::InProgress => (60, 200, 220), // cyan
        Status::Open => (200, 200, 80),       // yellow
        Status::Blocked => (180, 120, 60),    // orange
        Status::Abandoned => (140, 100, 160), // muted purple
        Status::Waiting => (60, 160, 220),    // blue
    }
}

/// Flash color for non-status changes.
fn flash_color_for_kind(kind: AnimationKind) -> (u8, u8, u8) {
    match kind {
        AnimationKind::NewTask => (220, 200, 80), // warm yellow
        AnimationKind::StatusChange => (200, 200, 200), // white (overridden by status)
        AnimationKind::ContentChange => (160, 160, 200), // soft blue-gray
        AnimationKind::Assignment => (200, 120, 220), // magenta
        AnimationKind::EdgeChange => (100, 180, 200), // teal
        AnimationKind::Revealed => (120, 120, 140),  // soft gray-blue
        AnimationKind::Evaluation => (80, 180, 220),  // cyan (matches ANSI 81)
        AnimationKind::Verification => (220, 190, 60), // gold (matches ANSI 220)
    }
}

/// Active lifecycle phase for a task (derived from system task states).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActivePhase {
    None,
    Assigning,
    Evaluating,
    Verifying,
}

/// Lightweight snapshot of per-task state for change detection.
#[derive(Clone, PartialEq, Eq)]
pub struct TaskSnapshot {
    pub status: Status,
    pub assigned: Option<String>,
    /// Token count bucketed to TOKEN_DEBOUNCE_BUCKET for debounce.
    pub token_bucket: u64,
    /// Number of dependency edges (after).
    pub edge_count: usize,
    /// Current lifecycle phase (from system task states).
    pub active_phase: ActivePhase,
}

// ══════════════════════════════════════════════════════════════════════════════
// Panel state types
// ══════════════════════════════════════════════════════════════════════════════

/// Which panel currently has keyboard focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusedPanel {
    Graph,
    RightPanel,
}

/// Which sub-zone within the inspector (right panel) has focus.
/// Only meaningful when the Chat tab is active and focused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InspectorSubFocus {
    /// Chat message history area — arrow keys scroll messages.
    ChatHistory,
    /// Text entry field — arrow keys move cursor within text.
    TextEntry,
}

/// Which tab is active in the right panel.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RightPanelTab {
    Chat,     // 0
    Detail,   // 1
    Log,      // 2
    Messages, // 3
    Agency,   // 4
    Config,   // 5
    Files,    // 6
    CoordLog, // 7
}

impl RightPanelTab {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Detail => "Detail",
            Self::Log => "Log",
            Self::Messages => "Msg",
            Self::Agency => "Agency",
            Self::Config => "Config",
            Self::Files => "Files",
            Self::CoordLog => "Coord",
        }
    }

    pub fn index(&self) -> usize {
        match self {
            Self::Chat => 0,
            Self::Detail => 1,
            Self::Log => 2,
            Self::Messages => 3,
            Self::Agency => 4,
            Self::Config => 5,
            Self::Files => 6,
            Self::CoordLog => 7,
        }
    }

    pub fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::Chat),
            1 => Some(Self::Detail),
            2 => Some(Self::Log),
            3 => Some(Self::Messages),
            4 => Some(Self::Agency),
            5 => Some(Self::Config),
            6 => Some(Self::Files),
            7 => Some(Self::CoordLog),
            _ => None,
        }
    }

    pub fn next(&self) -> Self {
        Self::from_index((self.index() + 1) % 8).unwrap()
    }

    pub fn prev(&self) -> Self {
        Self::from_index((self.index() + 7) % 8).unwrap()
    }

    pub const ALL: [RightPanelTab; 8] = [
        Self::Chat,
        Self::Detail,
        Self::Log,
        Self::Messages,
        Self::Agency,
        Self::Config,
        Self::Files,
        Self::CoordLog,
    ];
}

/// Which scrollbar is being dragged by the mouse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbarDragTarget {
    /// Dragging the graph pane scrollbar.
    Graph,
    /// Dragging the right panel scrollbar.
    Panel,
    /// Dragging the graph pane horizontal scrollbar.
    GraphHorizontal,
    /// Dragging the right panel horizontal scrollbar.
    #[allow(dead_code)]
    PanelHorizontal,
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
#[allow(dead_code)]
pub enum HudSize {
    /// ~1/3 of terminal (default).
    Normal,
    /// ~2/3 of terminal (expanded).
    Expanded,
}

#[allow(dead_code)]
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

/// Layout mode for the five-state cycle (i/I/=/Shift+Tab key).
/// Cycle: ThirdInspector → HalfInspector → TwoThirdsInspector → FullInspector → Off → ...
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayoutMode {
    /// 1/3 inspector (2/3 graph + 1/3 inspector).
    ThirdInspector,
    /// 1/2 inspector (1/2 graph + 1/2 inspector).
    HalfInspector,
    /// 2/3 inspector (1/3 graph + 2/3 inspector).
    #[default]
    TwoThirdsInspector,
    /// Full inspector: inspector takes entire screen, graph hidden.
    FullInspector,
    /// Off: graph takes entire screen, inspector hidden.
    Off,
}

impl LayoutMode {
    pub fn cycle(&self) -> Self {
        match self {
            Self::ThirdInspector => Self::HalfInspector,
            Self::HalfInspector => Self::TwoThirdsInspector,
            Self::TwoThirdsInspector => Self::FullInspector,
            Self::FullInspector => Self::Off,
            Self::Off => Self::ThirdInspector,
        }
    }

    pub fn cycle_reverse(&self) -> Self {
        match self {
            Self::ThirdInspector => Self::Off,
            Self::HalfInspector => Self::ThirdInspector,
            Self::TwoThirdsInspector => Self::HalfInspector,
            Self::FullInspector => Self::TwoThirdsInspector,
            Self::Off => Self::FullInspector,
        }
    }

    /// Convert a config string to a LayoutMode (for default inspector size).
    pub fn from_config_str(s: &str) -> Self {
        match s {
            "1/3" => Self::ThirdInspector,
            "1/2" => Self::HalfInspector,
            "2/3" => Self::TwoThirdsInspector,
            "full" => Self::FullInspector,
            _ => Self::TwoThirdsInspector,
        }
    }

    /// Convert to a config string.
    #[allow(dead_code)]
    pub fn to_config_str(self) -> &'static str {
        match self {
            Self::ThirdInspector => "1/3",
            Self::HalfInspector => "1/2",
            Self::TwoThirdsInspector => "2/3",
            Self::FullInspector => "full",
            Self::Off => "2/3", // Off isn't a valid default; fall back
        }
    }

    /// The panel percentage for this mode.
    pub fn panel_percent(&self) -> u16 {
        match self {
            Self::ThirdInspector => 33,
            Self::HalfInspector => 50,
            Self::TwoThirdsInspector => 67,
            Self::FullInspector => 100,
            Self::Off => 0,
        }
    }

    /// Whether this mode shows the inspector panel.
    pub fn has_inspector(&self) -> bool {
        matches!(
            self,
            Self::ThirdInspector
                | Self::HalfInspector
                | Self::TwoThirdsInspector
                | Self::FullInspector
        )
    }

    /// Whether this mode shows the graph.
    pub fn has_graph(&self) -> bool {
        matches!(
            self,
            Self::ThirdInspector | Self::HalfInspector | Self::TwoThirdsInspector | Self::Off
        )
    }
}

/// Input modes — at most one is active at a time.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Config panel text editing mode.
    ConfigEdit,
}

/// What action the confirmation dialog is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmAction {
    MarkDone(String), // task_id
    Retry(String),    // task_id
}

/// What action the text prompt dialog is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TextPromptAction {
    MarkFailed(String), // task_id
    #[allow(dead_code)]
    SendMessage(String), // task_id
    EditDescription(String), // task_id
    AttachFile,         // attach a file to the next chat message
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
            Ok(g) => g.tasks().map(|t| (t.id.clone(), t.title.clone())).collect(),
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
    /// Editor state for the input area.
    pub editor: EditorState,
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
    /// Pending attachments for the next message (file paths, already stored in .workgraph/attachments/).
    pub pending_attachments: Vec<PendingAttachment>,
    /// Total rendered lines (set each frame by renderer, for scrollbar dragging).
    pub total_rendered_lines: usize,
    /// Viewport height for the message area (set each frame by renderer).
    pub viewport_height: usize,
    /// Scroll offset from top (set each frame by renderer for scrollbar dragging).
    pub scroll_from_top: usize,
}

impl Default for ChatState {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            editor: new_emacs_editor(),
            scroll: 0,
            awaiting_response: false,
            outbox_cursor: 0,
            last_request_id: None,
            coordinator_active: false,
            pending_attachments: Vec::new(),
            total_rendered_lines: 0,
            viewport_height: 0,
            scroll_from_top: 0,
        }
    }
}

/// A pending attachment waiting to be sent with the next chat message.
#[allow(dead_code)]
pub struct PendingAttachment {
    /// Display filename (e.g. "screenshot.png").
    pub filename: String,
    /// Relative path to the stored copy (e.g. ".workgraph/attachments/20260303-...png").
    pub stored_path: String,
    /// MIME type.
    pub mime_type: String,
    /// Size in bytes.
    pub size_bytes: u64,
}

pub struct ChatMessage {
    pub role: ChatRole,
    pub text: String,
    /// Full response text including tool calls (coordinator messages only).
    /// Shown in expanded view instead of `text`.
    pub full_text: Option<String>,
    /// Attachment filenames for display (just the filename portion).
    pub attachments: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Coordinator,
    System,
}

/// Serializable chat message for persistence to disk.
#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedChatMessage {
    role: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    full_text: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    attachments: Vec<String>,
    timestamp: String,
}

/// Path to the persisted chat history file.
fn chat_history_path(workgraph_dir: &std::path::Path) -> std::path::PathBuf {
    workgraph_dir.join("chat-history.json")
}

/// Save chat messages to disk. Respects config for max history size.
fn save_chat_history(workgraph_dir: &std::path::Path, messages: &[ChatMessage]) {
    let config = Config::load_or_default(workgraph_dir);
    if !config.tui.chat_history {
        return;
    }
    let max = config.tui.chat_history_max;
    let skip = messages.len().saturating_sub(max);
    let persisted: Vec<PersistedChatMessage> = messages[skip..]
        .iter()
        .map(|m| PersistedChatMessage {
            role: match m.role {
                ChatRole::User => "user".to_string(),
                ChatRole::Coordinator => "coordinator".to_string(),
                ChatRole::System => "system".to_string(),
            },
            text: m.text.clone(),
            full_text: m.full_text.clone(),
            attachments: m.attachments.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
        })
        .collect();
    let path = chat_history_path(workgraph_dir);
    if let Ok(json) = serde_json::to_string(&persisted) {
        let _ = std::fs::write(&path, json);
    }
}

/// Load persisted chat history from disk.
fn load_persisted_chat_history(workgraph_dir: &std::path::Path) -> Vec<ChatMessage> {
    let config = Config::load_or_default(workgraph_dir);
    if !config.tui.chat_history {
        return vec![];
    }
    let path = chat_history_path(workgraph_dir);
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    let persisted: Vec<PersistedChatMessage> = match serde_json::from_str(&data) {
        Ok(p) => p,
        Err(_) => return vec![],
    };
    persisted
        .into_iter()
        .map(|p| ChatMessage {
            role: match p.role.as_str() {
                "user" => ChatRole::User,
                "coordinator" => ChatRole::Coordinator,
                _ => ChatRole::System,
            },
            text: p.text,
            full_text: p.full_text,
            attachments: p.attachments,
        })
        .collect()
}

/// State for the agent monitor panel.
#[derive(Default)]
pub struct AgentMonitorState {
    /// Agent entries loaded from the registry.
    pub agents: Vec<AgentMonitorEntry>,
    /// Scroll offset.
    pub scroll: usize,
    /// Total rendered lines (set each frame by renderer, for scrollbar dragging).
    pub total_rendered_lines: usize,
    /// Viewport height (set each frame by renderer).
    pub viewport_height: usize,
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

/// Live JSONL stream state for a single agent.
pub struct AgentStreamInfo {
    /// File position (byte offset) — resume reading from here.
    pub file_offset: u64,
    /// Total JSONL message count seen so far.
    pub message_count: usize,
    /// Latest content snippet (assistant text or tool use summary).
    pub latest_snippet: Option<String>,
    /// Whether the latest event was a tool use (vs text).
    pub latest_is_tool: bool,
}

/// A single phase in the agency lifecycle (assignment, execution, or evaluation).
pub struct LifecyclePhase {
    /// Task ID for this phase (e.g., "assign-foo", "foo", "evaluate-foo")
    pub task_id: String,
    /// Human label for the phase
    #[allow(dead_code)]
    pub label: &'static str,
    /// Status of this phase's task
    pub status: Status,
    /// Agent assigned to this phase (if any)
    pub agent_id: Option<String>,
    /// Token usage for this phase
    pub token_usage: Option<TokenUsage>,
    /// Runtime in seconds (if available)
    pub runtime_secs: Option<i64>,
    /// Evaluation score (only for evaluation phase)
    pub eval_score: Option<f64>,
    /// Evaluation notes (only for evaluation phase, first few lines)
    pub eval_notes: Option<String>,
}

/// Full agency lifecycle for a selected task: assignment → execution → evaluation.
pub struct AgencyLifecycle {
    /// The parent task ID this lifecycle is for.
    pub task_id: String,
    /// Assignment phase (if an assign-{task_id} task exists)
    pub assignment: Option<LifecyclePhase>,
    /// Execution phase (the task itself)
    pub execution: Option<LifecyclePhase>,
    /// Evaluation phase (if a .evaluate-{task_id} task exists)
    pub evaluation: Option<LifecyclePhase>,
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
    /// Total wrapped line count (set each frame by render, used for scroll bounds).
    pub total_wrapped_lines: usize,
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
            total_wrapped_lines: 0,
        }
    }
}

/// State for the Coordinator Log panel (panel 7) — shows daemon activity log.
pub struct CoordLogState {
    pub scroll: usize,
    pub auto_tail: bool,
    pub rendered_lines: Vec<String>,
    pub last_offset: u64,
    pub viewport_height: usize,
    pub total_wrapped_lines: usize,
}

impl Default for CoordLogState {
    fn default() -> Self {
        Self {
            scroll: 0,
            auto_tail: true,
            rendered_lines: Vec::new(),
            last_offset: 0,
            viewport_height: 0,
            total_wrapped_lines: 0,
        }
    }
}

/// Direction of a message relative to the task's assigned agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MessageDirection {
    /// Sent TO the task (by user, coordinator, or another agent).
    Incoming,
    /// Sent BY the task's assigned agent.
    Outgoing,
}

/// A single parsed message with direction metadata for rendering.
#[derive(Clone, Debug)]
pub struct MessageEntry {
    /// Sender identifier as stored in the message.
    pub sender: String,
    /// Display label (e.g., "you" for user/tui/coordinator).
    pub display_label: String,
    /// Message body text.
    pub body: String,
    /// Relative timestamp string (e.g., "2m ago").
    pub timestamp: String,
    /// Whether this is an urgent-priority message.
    pub is_urgent: bool,
    /// Direction: incoming to the task, or outgoing from the task's agent.
    pub direction: MessageDirection,
    /// Delivery status of this message.
    pub delivery_status: workgraph::messages::DeliveryStatus,
}

/// Summary stats for the messages panel header.
#[derive(Clone, Debug, Default)]
pub struct MessageSummary {
    /// Number of incoming messages (sent to the task).
    pub incoming: usize,
    /// Number of outgoing messages (sent by the task's agent).
    pub outgoing: usize,
    /// Whether the agent has responded after the latest incoming message.
    pub responded: bool,
    /// Number of incoming messages that have no subsequent outgoing reply.
    pub unanswered: usize,
}

/// State for the Messages panel (panel 3) — shows message queue for the selected task.
pub struct MessagesPanelState {
    /// Cached rendered log lines (kept for fallback/compat).
    pub rendered_lines: Vec<String>,
    /// Structured message entries with direction metadata.
    pub entries: Vec<MessageEntry>,
    /// Summary statistics for the header.
    pub summary: MessageSummary,
    /// Task ID these lines were rendered for (to detect staleness).
    pub task_id: Option<String>,
    /// Scroll offset.
    pub scroll: usize,
    /// Editor state for the input area.
    pub editor: EditorState,
    /// Total wrapped lines (set each frame by renderer, for scrollbar dragging).
    pub total_wrapped_lines: usize,
    /// Viewport height for the message area (set each frame by renderer).
    pub viewport_height: usize,
}

impl Default for MessagesPanelState {
    fn default() -> Self {
        Self {
            rendered_lines: Vec::new(),
            entries: Vec::new(),
            summary: MessageSummary::default(),
            task_id: None,
            scroll: 0,
            editor: new_emacs_editor(),
            total_wrapped_lines: 0,
            viewport_height: 0,
        }
    }
}

/// A single setting entry displayed in the Config panel.
pub struct ConfigEntry {
    /// Setting key used for saving (e.g., "coordinator.max_agents").
    pub key: String,
    /// Human-readable label for display.
    pub label: String,
    /// Current value as a displayable string.
    pub value: String,
    /// What kind of editing this entry supports.
    pub edit_kind: ConfigEditKind,
    /// Section this entry belongs to.
    pub section: ConfigSection,
}

/// How a config entry can be edited.
pub enum ConfigEditKind {
    /// Simple boolean toggle.
    Toggle,
    /// Choose from a fixed list of options.
    Choice(Vec<String>),
    /// Free-form text/number input.
    TextInput,
    /// Sensitive text input (API key — shows masked).
    SecretInput,
}

/// Config dashboard sections.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ConfigSection {
    Endpoints,
    ApiKeys,
    Service,
    TuiSettings,
    AgentDefaults,
    Agency,
    Guardrails,
}

impl ConfigSection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Endpoints => "LLM Endpoints",
            Self::ApiKeys => "API Keys",
            Self::Service => "Service Settings",
            Self::TuiSettings => "TUI Settings",
            Self::AgentDefaults => "Agent Defaults",
            Self::Agency => "Agency",
            Self::Guardrails => "Guardrails",
        }
    }

    #[allow(dead_code)]
    pub fn all() -> &'static [ConfigSection] {
        &[
            Self::Endpoints,
            Self::ApiKeys,
            Self::Service,
            Self::TuiSettings,
            Self::AgentDefaults,
            Self::Agency,
            Self::Guardrails,
        ]
    }
}

/// State for the Config panel (panel 5).
#[derive(Default)]
pub struct ConfigPanelState {
    /// All config entries to display.
    pub entries: Vec<ConfigEntry>,
    /// Currently selected entry index.
    pub selected: usize,
    /// Scroll offset for the list.
    pub scroll: usize,
    /// Whether we're currently editing the selected entry.
    pub editing: bool,
    /// Input buffer when editing a TextInput entry.
    pub edit_buffer: String,
    /// Selected choice index when editing a Choice entry.
    pub choice_index: usize,
    /// Notification message (e.g., "Saved!" shown briefly).
    pub save_notification: Option<std::time::Instant>,
    /// Which sections are collapsed.
    pub collapsed: std::collections::HashSet<ConfigSection>,
    /// Whether we're in the "add endpoint" flow.
    pub adding_endpoint: bool,
    /// Fields for new endpoint being added.
    pub new_endpoint: NewEndpointFields,
    /// Which field in the new-endpoint form is active (0-4).
    pub new_endpoint_field: usize,
    /// Service running status (cached, updated on load).
    pub service_running: bool,
    /// Service PID if running.
    pub service_pid: Option<u32>,
}

/// Fields for the "add new endpoint" form.
#[derive(Default, Clone)]
pub struct NewEndpointFields {
    pub name: String,
    pub provider: String,
    pub url: String,
    pub model: String,
    pub api_key: String,
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
    #[allow(dead_code)]
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
    pub editor: EditorState,
}

/// Loaded detail for the HUD panel showing info about the selected task.
#[derive(Default)]
pub struct HudDetail {
    /// Task ID this detail was loaded for (to detect stale data).
    pub task_id: String,
    /// All content lines assembled for rendering (with section headers).
    pub rendered_lines: Vec<String>,
}

/// Extract section name from a detail header line like "── Description ──" → "Description".
/// Also handles lines with trailing annotations: "── Output ── [R: raw JSON]" → "Output".
/// Returns None for non-header lines or the task-id header (first line).
pub fn extract_section_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with("──") {
        return None;
    }
    // Strip trailing annotations like " [R: raw JSON]" before checking the closing ──.
    let base = trimmed.split(" [").next().unwrap_or(trimmed).trim_end();
    if !base.ends_with("──") {
        return None;
    }
    let inner = base
        .trim_start_matches('─')
        .trim_end_matches('─')
        .trim();
    // Section names are things like "Description", "Prompt", "Output", "Output (raw)", etc.
    if !inner.is_empty() {
        Some(inner.to_string())
    } else {
        None
    }
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

    // ── System task visibility ──
    /// When true, show system tasks (dot-prefixed) in the graph view.
    pub show_system_tasks: bool,
    /// Set to true when system task visibility was just toggled, so that
    /// newly appearing tasks get a `Revealed` animation instead of `NewTask`.
    pub system_tasks_just_toggled: bool,

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
    /// The chat input area from the last render frame (for click-to-resume editing).
    pub last_chat_input_area: Rect,
    /// The chat message history area from the last render frame (for click-to-focus).
    pub last_chat_message_area: Rect,

    /// The text prompt overlay area from the last render frame (for mouse scroll).
    pub last_text_prompt_area: Rect,

    /// The file browser tree pane area from the last render frame (for mouse clicks).
    pub last_file_tree_area: Rect,
    /// The file browser preview pane area from the last render frame (for mouse clicks).
    pub last_file_preview_area: Rect,

    /// Maps config entry index → screen Y position (set each frame by renderer).
    /// Used for mouse click → config entry selection.
    pub config_entry_y_positions: Vec<(usize, u16)>,

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
    /// Active lifecycle phase annotations: parent_task_id → list of phases.
    pub phase_annotations: HashMap<String, Vec<crate::commands::viz::PhaseAnnotation>>,

    // ── HUD (info panel) ──
    /// Loaded HUD detail for the currently selected task.
    pub hud_detail: Option<HudDetail>,
    /// Scroll offset within the HUD panel (vertical).
    pub hud_scroll: usize,
    /// Total wrapped line count in the detail panel (set by renderer each frame).
    pub hud_wrapped_line_count: usize,
    /// Viewport height of the detail panel (set by renderer each frame).
    pub hud_detail_viewport_height: usize,
    /// When true, show raw JSON in the Detail tab instead of human-readable format.
    pub detail_raw_json: bool,
    /// Set of collapsed section names in the Detail view (persists across task switches).
    pub detail_collapsed_sections: std::collections::HashSet<String>,
    /// Map from wrapped line index → section name for section headers in the Detail view.
    /// Populated each frame by the renderer; used for mouse click hit-testing.
    pub detail_section_header_lines: Vec<(usize, String)>,

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
    /// Layout mode for five-state cycle (1/3 → 1/2 → 2/3 → full → off).
    pub layout_mode: LayoutMode,
    /// Current input mode.
    pub input_mode: InputMode,
    /// Deferred centering: set during state refresh, consumed by render after viewport_height is known.
    pub needs_center_on_selected: bool,
    /// Deferred scroll-into-view: only scrolls if the task is off-screen (centers when it does).
    pub needs_scroll_into_view: bool,
    /// True when user explicitly dismissed chat input with Esc.
    /// Prevents auto-re-entering ChatInput until user navigates away from Chat tab.
    pub chat_input_dismissed: bool,
    /// Which sub-zone within the inspector has focus (chat history vs text entry).
    pub inspector_sub_focus: InspectorSubFocus,

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
    /// Per-agent JSONL stream state for live activity feed.
    pub agent_streams: HashMap<String, AgentStreamInfo>,

    // ── Agency lifecycle for selected task ──
    pub agency_lifecycle: Option<AgencyLifecycle>,

    // ── Log pane state (now embedded as panel 2) ──
    pub log_pane: LogPaneState,

    // ── Coordinator log state (panel 7) ──
    pub coord_log: CoordLogState,

    // ── Messages panel state (panel 3) ──
    pub messages_panel: MessagesPanelState,

    // ── Config panel state (panel 5) ──
    pub config_panel: ConfigPanelState,

    // ── File browser state (panel 6) ──
    pub file_browser: Option<super::file_browser::FileBrowser>,

    // ── Command queue ──
    /// Channel receiver for background command results.
    pub cmd_rx: mpsc::Receiver<CommandResult>,
    /// Channel sender (cloned into background threads).
    pub cmd_tx: mpsc::Sender<CommandResult>,
    /// Notification message to display (transient, cleared after a few seconds).
    pub notification: Option<(String, Instant)>,

    // ── Double-tap detection ──
    /// Timestamp of the last Tab key press, for double-tap recenter detection.
    #[allow(dead_code)]
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

    // ── Animations ──
    /// Active flash-and-fade animations keyed by task ID.
    /// Each task can have at most one active animation; newer changes replace older ones.
    pub splash_animations: HashMap<String, Animation>,
    /// Previous per-task snapshots for change detection.
    /// Populated on each refresh; compared to current state to detect changes.
    pub task_snapshots: HashMap<String, TaskSnapshot>,
    /// Animation mode (from config: normal/fast/slow/reduced/off).
    pub animation_mode: AnimationMode,
    /// Cached: name length threshold for inline vs above-line display.
    pub message_name_threshold: u16,
    /// Cached: indent for message body when name is on its own line.
    pub message_indent: u16,

    // ── Scrollbar auto-hide (per-pane) ──
    /// Timestamp of the last scroll activity in the graph pane.
    pub graph_scroll_activity: Option<Instant>,
    /// Timestamp of the last scroll activity in the right panel.
    pub panel_scroll_activity: Option<Instant>,

    // ── Scrollbar drag state ──
    /// Which scrollbar (if any) is currently being dragged.
    pub scrollbar_drag: Option<ScrollbarDragTarget>,

    /// Vertical scrollbar area for the graph pane (set each frame by renderer).
    pub last_graph_scrollbar_area: Rect,
    /// Vertical scrollbar area for the right panel (set each frame by renderer).
    pub last_panel_scrollbar_area: Rect,

    pub graph_hscroll_activity: Option<Instant>,
    #[allow(dead_code)]
    pub panel_hscroll_activity: Option<Instant>,
    pub last_graph_hscrollbar_area: Rect,
    #[allow(dead_code)]
    pub last_panel_hscrollbar_area: Rect,

    // ── Keyboard enhancement ──
    /// Whether the kitty keyboard protocol was successfully enabled.
    /// When true, Shift+Enter is distinguishable from Enter.
    pub has_keyboard_enhancement: bool,

    pub editor_handler: EditorEventHandler,

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
        let config = Config::load_or_default(&workgraph_dir);
        let animation_mode = AnimationMode::from_config(&config.viz.animations);
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
            show_system_tasks: false,
            system_tasks_just_toggled: false,
            mouse_enabled,
            last_graph_area: Rect::default(),
            last_right_panel_area: Rect::default(),
            last_tab_bar_area: Rect::default(),
            last_right_content_area: Rect::default(),
            last_chat_input_area: Rect::default(),
            last_chat_message_area: Rect::default(),
            last_text_prompt_area: Rect::default(),
            last_file_tree_area: Rect::default(),
            last_file_preview_area: Rect::default(),
            config_entry_y_positions: Vec::new(),
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
            phase_annotations: HashMap::new(),
            hud_detail: None,
            hud_scroll: 0,
            hud_wrapped_line_count: 0,
            hud_detail_viewport_height: 0,
            detail_raw_json: false,
            detail_collapsed_sections: ["Output", "Output (raw)", "Prompt"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            detail_section_header_lines: Vec::new(),
            right_panel_visible: true,
            focused_panel: FocusedPanel::Graph,
            right_panel_tab: RightPanelTab::Chat,
            right_panel_percent: LayoutMode::from_config_str(&config.tui.default_inspector_size)
                .panel_percent(),
            hud_size: HudSize::Normal,
            layout_mode: LayoutMode::from_config_str(&config.tui.default_inspector_size),

            input_mode: InputMode::Normal,
            needs_center_on_selected: false,
            needs_scroll_into_view: false,
            chat_input_dismissed: false,
            inspector_sub_focus: InspectorSubFocus::ChatHistory,
            task_form: None,
            text_prompt: TextPromptState {
                editor: new_emacs_editor(),
            },
            chat: ChatState::default(),
            agent_monitor: AgentMonitorState::default(),
            agent_streams: HashMap::new(),
            agency_lifecycle: None,
            log_pane: LogPaneState::default(),
            coord_log: CoordLogState::default(),
            messages_panel: MessagesPanelState::default(),
            config_panel: ConfigPanelState::default(),
            file_browser: None,
            cmd_rx,
            cmd_tx,
            notification: None,
            last_tab_press: None,
            sort_mode: SortMode::Chronological,
            smart_follow_active: true,
            initial_load: true,
            splash_animations: HashMap::new(),
            task_snapshots: HashMap::new(),
            animation_mode,
            message_name_threshold: config.tui.message_name_threshold,
            message_indent: config.tui.message_indent,
            graph_scroll_activity: None,
            panel_scroll_activity: None,
            scrollbar_drag: None,
            last_graph_scrollbar_area: Rect::default(),
            last_panel_scrollbar_area: Rect::default(),
            graph_hscroll_activity: None,
            panel_hscroll_activity: None,
            last_graph_hscrollbar_area: Rect::default(),
            last_panel_hscrollbar_area: Rect::default(),
            has_keyboard_enhancement: false,
            editor_handler: create_editor_handler(),
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

                // Detect newly appeared tasks and register splash animations.
                // Skip on initial load (old_task_order is empty).
                if !old_task_order.is_empty() && self.animation_mode.is_enabled() {
                    let old_set: HashSet<&str> =
                        old_task_order.iter().map(|s| s.as_str()).collect();
                    let now = Instant::now();
                    // If system task visibility was just toggled, newly visible
                    // tasks get a gentle "revealed" animation instead of the
                    // bright "new task" flash.
                    let anim_kind = if self.system_tasks_just_toggled {
                        AnimationKind::Revealed
                    } else {
                        AnimationKind::NewTask
                    };
                    for id in &self.task_order {
                        if !old_set.contains(id.as_str())
                            && !self.splash_animations.contains_key(id)
                        {
                            self.splash_animations.insert(
                                id.clone(),
                                Animation {
                                    start: now,
                                    flash_color: flash_color_for_kind(anim_kind),
                                    kind: anim_kind,
                                },
                            );
                        }
                    }
                    self.system_tasks_just_toggled = false;

                    // Note: per-task content changes (status, assignment, edges,
                    // tokens) are detected by field-level comparison in
                    // load_stats(). We intentionally do NOT compare rendered
                    // plain-line text here, because tree connector characters
                    // and duration text change whenever tasks shift position
                    // or minutes tick over, causing false-positive flashes on
                    // stable tasks.
                }

                // Re-apply the current sort mode so task_order reflects the
                // user's selected ordering (e.g. StatusGrouped) immediately,
                // rather than staying in raw viz line order until the next
                // manual sort-cycle.  This prevents new tasks from briefly
                // appearing at their dependency position before jumping to
                // their correct sorted position on the next refresh.
                self.apply_sort_mode();

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
                    let anchored = old_relative_pos.and_then(|rel_pos| {
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
                } else if new_task_focused {
                    // New task appeared — defer centering until render sets viewport_height.
                    self.needs_center_on_selected = true;
                } else {
                    // Selection changed (different task) — only scroll if off-screen.
                    self.needs_scroll_into_view = true;
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
                self.phase_annotations.clear();
                self.update_scroll_bounds();
            }
        }
    }

    fn generate_viz(&self) -> Result<VizOutput> {
        let mut opts = self.viz_options.clone();
        // System tasks are never shown as separate nodes in the dot view.
        // Their lifecycle state is shown as phase indicators on the parent node.
        opts.show_internal = false;
        crate::commands::viz::generate_viz_output(&self.workgraph_dir, &opts)
    }

    /// Update scroll content bounds based on current filter state.
    pub fn update_scroll_bounds(&mut self) {
        let height = match &self.filtered_indices {
            Some(indices) => indices.len(),
            None => self.lines.len(),
        };
        // Add 1 for an empty padding row at the bottom, giving visual breathing
        // room so the last task isn't flush against the viewport edge.
        self.scroll.content_height = height + 1;
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

    /// Move task selection up by `n` tasks in the viz order.
    pub fn select_prev_task_n(&mut self, n: usize) {
        if self.task_order.is_empty() {
            return;
        }
        let idx = match self.selected_task_idx {
            Some(i) => i.saturating_sub(n),
            None => 0,
        };
        self.selected_task_idx = Some(idx);
        self.recompute_trace();
        self.scroll_to_selected_task();
    }

    /// Move task selection down by `n` tasks in the viz order.
    pub fn select_next_task_n(&mut self, n: usize) {
        if self.task_order.is_empty() {
            return;
        }
        let last = self.task_order.len() - 1;
        let idx = match self.selected_task_idx {
            Some(i) => (i + n).min(last),
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

        // Invalidate HUD, lifecycle, and messages panel so they reload for the new selection.
        self.invalidate_hud();
        self.invalidate_agency_lifecycle();
        self.invalidate_log_pane();
        self.invalidate_messages_panel();
    }

    /// Scroll the viewport so the selected task stays within the middle 60% of
    /// the viewport (a "comfort zone"). If the task is in the top or bottom 20%,
    /// recenter it to the middle — similar to vim's `scrolloff`.
    pub fn scroll_to_selected_task(&mut self) {
        let task_id = match self.selected_task_idx.and_then(|i| self.task_order.get(i)) {
            Some(id) => id,
            None => return,
        };
        let orig_line = match self.node_line_map.get(task_id) {
            Some(&line) => line,
            None => return,
        };
        if let Some(visible_pos) = self.original_to_visible(orig_line) {
            let vh = self.scroll.viewport_height;
            let margin = vh / 5; // 20% margin top and bottom
            let comfort_top = self.scroll.offset_y + margin;
            let comfort_bottom = self.scroll.offset_y + vh.saturating_sub(margin);
            if visible_pos < comfort_top || visible_pos >= comfort_bottom {
                let half = vh / 2;
                self.scroll.offset_y = visible_pos.saturating_sub(half);
                self.scroll.clamp();
            }
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
            self.notification = Some((format!("New task: {}", task_id), Instant::now()));
            return false;
        }

        // Find the task in the current task_order.
        if let Some(idx) = self.task_order.iter().position(|id| id == &task_id) {
            self.selected_task_idx = Some(idx);
            // The splash animation (registered earlier in refresh_data) provides
            // the visual highlight for new tasks. Don't also set jump_target here
            // — its 2s lifetime outlasts the 1.5s splash, causing a yellow flash
            // after the smooth fade finishes.
            self.notification = Some((format!("New task: {}", task_id), Instant::now()));
            true
        } else {
            false
        }
    }

    // ── Splash animations ──

    /// Returns the fade progress for a task's splash animation.
    /// 0.0 = animation just started (full brightness), 1.0 = fully faded.
    /// Returns None if the task has no active animation.
    pub fn splash_progress(&self, task_id: &str) -> Option<f64> {
        let anim = self.splash_animations.get(task_id)?;
        let duration = self.animation_mode.speed().duration_secs();
        let elapsed = anim.start.elapsed().as_secs_f64();
        Some((elapsed / duration).min(1.0))
    }

    /// Returns the flash color for a task's active animation, or None.
    pub fn splash_color(&self, task_id: &str) -> Option<(u8, u8, u8)> {
        self.splash_animations.get(task_id).map(|a| a.flash_color)
    }

    /// Returns the animation kind for a task's active animation, or None.
    pub fn splash_kind(&self, task_id: &str) -> Option<AnimationKind> {
        self.splash_animations.get(task_id).map(|a| a.kind)
    }

    /// Whether any splash animations are currently active (not yet fully faded).
    #[allow(dead_code)]
    pub fn has_active_animations(&self) -> bool {
        let duration = self.animation_mode.speed().duration_secs();
        let cutoff = std::time::Duration::from_secs_f64(duration);
        self.splash_animations
            .values()
            .any(|anim| anim.start.elapsed() < cutoff)
    }

    /// Remove expired splash animations.
    pub fn cleanup_splash_animations(&mut self) {
        let duration = self.animation_mode.speed().duration_secs();
        let cutoff = std::time::Duration::from_secs_f64(duration);
        self.splash_animations
            .retain(|_, anim| anim.start.elapsed() < cutoff);
    }

    /// Enforce the maximum animation count by dropping oldest animations.
    fn enforce_animation_cap(&mut self) {
        if self.splash_animations.len() <= MAX_ANIMATIONS {
            return;
        }
        // Collect (id, start_time) and sort by start time ascending (oldest first).
        let mut entries: Vec<(String, Instant)> = self
            .splash_animations
            .iter()
            .map(|(k, a)| (k.clone(), a.start))
            .collect();
        entries.sort_by_key(|(_, t)| *t);
        // Remove oldest until we're at the cap.
        let to_remove = entries.len() - MAX_ANIMATIONS;
        for (id, _) in entries.into_iter().take(to_remove) {
            self.splash_animations.remove(&id);
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

        // Build phase_map: parent_task_id → ActivePhase for in-progress system tasks.
        let phase_map: HashMap<String, ActivePhase> = {
            let mut map = HashMap::new();
            for task in graph.tasks() {
                if task.status != Status::InProgress {
                    continue;
                }
                let id = &task.id;
                if let Some(parent_id) = crate::commands::viz::system_task_parent_id(id) {
                    let phase = if id.starts_with(".assign-") || id.starts_with("assign-") {
                        ActivePhase::Assigning
                    } else if id.starts_with(".verify-") || id.starts_with("verify-") {
                        ActivePhase::Verifying
                    } else {
                        ActivePhase::Evaluating
                    };
                    map.insert(parent_id, phase);
                }
            }
            map
        };

        let mut new_snapshots: HashMap<String, TaskSnapshot> = HashMap::new();
        let now = Instant::now();

        for task in graph.tasks() {
            counts.total += 1;
            match task.status {
                Status::Done => counts.done += 1,
                Status::Open => counts.open += 1,
                Status::InProgress => counts.in_progress += 1,
                Status::Failed => counts.failed += 1,
                Status::Blocked => counts.blocked += 1,
                Status::Abandoned => counts.done += 1, // count with done
                Status::Waiting => counts.blocked += 1, // count with blocked
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

            // Build snapshot for change detection.
            let total_tokens = usage.map(|u| u.input_tokens + u.output_tokens).unwrap_or(0);

            let active_phase = phase_map
                .get(&task.id)
                .copied()
                .unwrap_or(ActivePhase::None);

            let snapshot = TaskSnapshot {
                status: task.status,
                assigned: task.assigned.clone(),
                token_bucket: total_tokens / TOKEN_DEBOUNCE_BUCKET,
                edge_count: task.after.len(),
                active_phase,
            };

            // Compare with previous snapshot and create animations for changes.
            if self.animation_mode.is_enabled()
                && let Some(old) = self.task_snapshots.get(&task.id)
            {
                // Status change — most important, overrides other animations.
                if old.status != snapshot.status {
                    self.splash_animations.insert(
                        task.id.clone(),
                        Animation {
                            start: now,
                            flash_color: flash_color_for_status(&snapshot.status),
                            kind: AnimationKind::StatusChange,
                        },
                    );
                }
                // Agent assignment change.
                else if old.assigned != snapshot.assigned && snapshot.assigned.is_some() {
                    self.splash_animations.insert(
                        task.id.clone(),
                        Animation {
                            start: now,
                            flash_color: flash_color_for_kind(AnimationKind::Assignment),
                            kind: AnimationKind::Assignment,
                        },
                    );
                }
                // Edge count change (new dependency).
                else if old.edge_count != snapshot.edge_count
                    && !self.splash_animations.contains_key(&task.id)
                {
                    self.splash_animations.insert(
                        task.id.clone(),
                        Animation {
                            start: now,
                            flash_color: flash_color_for_kind(AnimationKind::EdgeChange),
                            kind: AnimationKind::EdgeChange,
                        },
                    );
                }
                // Token usage changed significantly (debounced by bucket).
                else if old.token_bucket != snapshot.token_bucket
                    && !self.splash_animations.contains_key(&task.id)
                {
                    self.splash_animations.insert(
                        task.id.clone(),
                        Animation {
                            start: now,
                            flash_color: flash_color_for_kind(AnimationKind::ContentChange),
                            kind: AnimationKind::ContentChange,
                        },
                    );
                }
                // Lifecycle phase changed (e.g. assigning → evaluating → verifying).
                else if old.active_phase != snapshot.active_phase
                    && snapshot.active_phase != ActivePhase::None
                    && !self.splash_animations.contains_key(&task.id)
                {
                    let anim_kind = match snapshot.active_phase {
                        ActivePhase::Assigning => AnimationKind::Assignment,
                        ActivePhase::Evaluating => AnimationKind::Evaluation,
                        ActivePhase::Verifying => AnimationKind::Verification,
                        ActivePhase::None => unreachable!(),
                    };
                    self.splash_animations.insert(
                        task.id.clone(),
                        Animation {
                            start: now,
                            flash_color: flash_color_for_kind(anim_kind),
                            kind: anim_kind,
                        },
                    );
                }
                // Note: new tasks (not in old snapshots) are already handled in load_viz().
            }

            new_snapshots.insert(task.id.clone(), snapshot);
        }

        self.task_snapshots = new_snapshots;
        self.task_counts = counts;
        self.total_usage = total_usage;
        self.task_token_map = task_token_map;

        // Enforce animation cap: drop oldest if we exceed MAX_ANIMATIONS.
        self.enforce_animation_cap();
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
            // Capture HUD scroll state BEFORE load_viz(), because load_viz() ->
            // recompute_trace() -> invalidate_hud() clears hud_detail.
            let prev_hud_task = self.hud_detail.as_ref().map(|d| d.task_id.clone());
            let prev_hud_scroll = self.hud_scroll;

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
            self.update_agent_streams();
            // Preserve HUD scroll position when the selected task hasn't changed.
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
            // Reload agency lifecycle if Agency tab is active.
            if self.right_panel_tab == RightPanelTab::Agency {
                self.invalidate_agency_lifecycle();
                self.load_agency_lifecycle();
            }
            // Refresh file browser tree if Files tab is active.
            if self.right_panel_tab == RightPanelTab::Files
                && let Some(ref mut fb) = self.file_browser
            {
                fb.refresh();
            }
            // Refresh coordinator log if CoordLog tab is active.
            if self.right_panel_tab == RightPanelTab::CoordLog {
                self.load_coord_log();
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
    #[allow(dead_code)]
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
        self.notification = Some((format!("Sort: {}", self.sort_mode.label()), Instant::now()));
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
                self.task_order
                    .sort_by_key(|id| self.node_line_map.get(id).copied().unwrap_or(usize::MAX));
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
                                Status::Waiting => 3, // same priority as blocked
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
            for line in desc.lines() {
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
                for l in reader.lines().map_while(Result::ok) {
                    lines.push(format!("  {}", l));
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
            if self.detail_raw_json {
                lines.push("── Output (raw) ── [R: human-readable]".to_string());
            } else {
                lines.push("── Output ── [R: raw JSON]".to_string());
            }
            if let Ok(content) = std::fs::read_to_string(&output_path) {
                for line in content.lines() {
                    let trimmed = line.trim();
                    if (trimmed.starts_with('{') || trimmed.starts_with('['))
                        && let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
                    {
                        if self.detail_raw_json {
                            // Raw mode: pretty-printed JSON
                            if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                                for pline in pretty.lines() {
                                    lines.push(format!("  {}", pline));
                                }
                            } else {
                                lines.push(format!("  {}", line));
                            }
                        } else {
                            // Human-readable mode: extract key/value pairs
                            flatten_json_to_lines(&val, "  ", &mut lines);
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

        // ── Token usage (execution) ──
        if let Some(ref usage) = task.token_usage {
            lines.push("── Tokens ──".to_string());
            let cache_total = usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
            if cache_total > 0 {
                lines.push(format!(
                    "  Input:  {} new + {} cached",
                    format_tokens(usage.input_tokens),
                    format_tokens(cache_total)
                ));
            } else {
                lines.push(format!("  Input:  {}", format_tokens(usage.input_tokens)));
            }
            lines.push(format!("  Output: {}", format_tokens(usage.output_tokens)));
            if usage.cache_read_input_tokens > 0 || usage.cache_creation_input_tokens > 0 {
                lines.push(format!(
                    "  Cache read:  {}",
                    format_tokens(usage.cache_read_input_tokens)
                ));
                lines.push(format!(
                    "  Cache write: {}",
                    format_tokens(usage.cache_creation_input_tokens)
                ));
            }
            if usage.cost_usd > 0.0 {
                lines.push(format!("  Cost: ${:.4}", usage.cost_usd));
            }
            lines.push(String::new());
        }

        // ── Assignment + Evaluation costs ──
        {
            let agents_dir = self.workgraph_dir.join("agents");
            let assign_task_id = format!(".assign-{}", task.id);
            let legacy_assign_id = format!("assign-{}", task.id);
            let eval_task_id = format!(".evaluate-{}", task.id);
            let legacy_eval_id = format!("evaluate-{}", task.id);

            let assign_usage = graph
                .tasks()
                .find(|t| t.id == assign_task_id || t.id == legacy_assign_id)
                .and_then(|t| {
                    t.token_usage.clone().or_else(|| {
                        let agent_id = t.assigned.as_deref()?;
                        let log_path = agents_dir.join(agent_id).join("output.log");
                        parse_token_usage_live(&log_path)
                    })
                });
            let eval_usage = graph
                .tasks()
                .find(|t| t.id == eval_task_id || t.id == legacy_eval_id)
                .and_then(|t| {
                    t.token_usage.clone().or_else(|| {
                        let agent_id = t.assigned.as_deref()?;
                        let log_path = agents_dir.join(agent_id).join("output.log");
                        parse_token_usage_live(&log_path)
                    })
                });

            if assign_usage.is_some() || eval_usage.is_some() {
                lines.push("── Phase Costs ──".to_string());
                if let Some(ref u) = assign_usage {
                    let cache = u.cache_read_input_tokens + u.cache_creation_input_tokens;
                    let mut detail = format!(
                        "  ⊳ Assignment: →{} ←{}",
                        format_tokens(u.input_tokens),
                        format_tokens(u.output_tokens)
                    );
                    if cache > 0 {
                        detail.push_str(&format!(" +{} cached", format_tokens(cache)));
                    }
                    if u.cost_usd > 0.0 {
                        detail.push_str(&format!(" ${:.4}", u.cost_usd));
                    }
                    lines.push(detail);
                }
                if let Some(ref u) = eval_usage {
                    let cache = u.cache_read_input_tokens + u.cache_creation_input_tokens;
                    let mut detail = format!(
                        "  ∴ Evaluation: →{} ←{}",
                        format_tokens(u.input_tokens),
                        format_tokens(u.output_tokens)
                    );
                    if cache > 0 {
                        detail.push_str(&format!(" +{} cached", format_tokens(cache)));
                    }
                    if u.cost_usd > 0.0 {
                        detail.push_str(&format!(" ${:.4}", u.cost_usd));
                    }
                    lines.push(detail);
                }
                // Show combined total
                let exec_cost = task.token_usage.as_ref().map(|u| u.cost_usd).unwrap_or(0.0);
                let total_cost = exec_cost
                    + assign_usage.as_ref().map(|u| u.cost_usd).unwrap_or(0.0)
                    + eval_usage.as_ref().map(|u| u.cost_usd).unwrap_or(0.0);
                if total_cost > 0.0 {
                    lines.push(format!("  Total cost: ${:.4}", total_cost));
                }
                lines.push(String::new());
            }
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

    /// Load HUD detail for an arbitrary task ID (used for navigating to internal tasks
    /// like assign-* and evaluate-* from the Agency tab).
    pub fn load_hud_detail_for_task(&mut self, target_task_id: &str) {
        self.hud_scroll = 0;

        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = match load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => {
                self.hud_detail = None;
                return;
            }
        };

        let task = match graph.tasks().find(|t| t.id == target_task_id) {
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
            for line in desc.lines() {
                lines.push(format!("  {}", line));
            }
            lines.push(String::new());
        }

        // ── Agent output ──
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
                        && let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed)
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

        // ── Token usage ──
        if let Some(ref usage) = task.token_usage {
            lines.push("── Tokens ──".to_string());
            let cache_total = usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
            if cache_total > 0 {
                lines.push(format!(
                    "  Input:  {} new + {} cached",
                    format_tokens(usage.input_tokens),
                    format_tokens(cache_total)
                ));
            } else {
                lines.push(format!("  Input:  {}", format_tokens(usage.input_tokens)));
            }
            lines.push(format!("  Output: {}", format_tokens(usage.output_tokens)));
            if usage.cache_read_input_tokens > 0 || usage.cache_creation_input_tokens > 0 {
                lines.push(format!(
                    "  Cache read:  {}",
                    format_tokens(usage.cache_read_input_tokens)
                ));
                lines.push(format!(
                    "  Cache write: {}",
                    format_tokens(usage.cache_creation_input_tokens)
                ));
            }
            if usage.cost_usd > 0.0 {
                lines.push(format!("  Cost: ${:.4}", usage.cost_usd));
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

        self.hud_detail = Some(HudDetail {
            task_id: target_task_id.to_string(),
            rendered_lines: lines,
        });
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

    /// Toggle collapse state of the section header at the current scroll position.
    /// Returns the name of the toggled section, if any.
    pub fn toggle_detail_section_at_scroll(&mut self) -> Option<String> {
        let detail = self.hud_detail.as_ref()?;
        // Find which section header is currently at the top of the viewport (or closest above).
        // We use the unwrapped rendered_lines to find section headers, then map
        // through the wrapped lines to find the one at the scroll position.
        // Since we can't easily map wrapped→unwrapped here, we scan the wrapped
        // output that was last produced by draw_detail_tab. Instead, we'll just
        // scan rendered_lines for section headers and let the renderer handle it.
        // Find the section at the current scroll position by scanning rendered_lines.
        let mut current_section: Option<String> = None;
        for (line_idx, raw_line) in detail.rendered_lines.iter().enumerate() {
            if let Some(name) = extract_section_name(raw_line) {
                // This is a section header. Check if it's the first one at or before scroll pos.
                current_section = Some(name);
            }
            if line_idx >= self.hud_scroll {
                break;
            }
        }
        if let Some(ref name) = current_section {
            if self.detail_collapsed_sections.contains(name) {
                self.detail_collapsed_sections.remove(name);
            } else {
                self.detail_collapsed_sections.insert(name.clone());
            }
        }
        current_section
    }

    /// Toggle collapse state of a section by name.
    pub fn toggle_detail_section_by_name(&mut self, name: &str) {
        if self.detail_collapsed_sections.contains(name) {
            self.detail_collapsed_sections.remove(name);
        } else {
            self.detail_collapsed_sections.insert(name.to_string());
        }
    }

    /// Toggle the section header at the given screen row (relative to the detail content area).
    /// Uses the section_header_positions populated by the renderer.
    /// Returns the section name if a header was found and toggled.
    pub fn toggle_detail_section_at_screen_row(&mut self, screen_row: usize) -> Option<String> {
        let line_idx = self.hud_scroll + screen_row;
        // Find a header at this wrapped line index.
        let name = self
            .detail_section_header_lines
            .iter()
            .find(|(idx, _)| *idx == line_idx)
            .map(|(_, name)| name.clone())?;
        self.toggle_detail_section_by_name(&name);
        Some(name)
    }

    /// Record scroll activity in the graph pane for auto-hiding scrollbar.
    pub fn record_graph_scroll_activity(&mut self) {
        self.graph_scroll_activity = Some(Instant::now());
    }

    /// Record scroll activity in the right panel for auto-hiding scrollbar.
    pub fn record_panel_scroll_activity(&mut self) {
        self.panel_scroll_activity = Some(Instant::now());
    }

    /// Whether the graph pane scrollbar should be visible (within 2s of last graph scroll,
    /// or while actively dragging the graph scrollbar).
    pub fn graph_scrollbar_visible(&self) -> bool {
        if self.scrollbar_drag == Some(ScrollbarDragTarget::Graph) {
            return true;
        }
        match self.graph_scroll_activity {
            Some(when) => when.elapsed() < std::time::Duration::from_secs(2),
            None => false,
        }
    }

    /// Whether the right panel scrollbar should be visible (within 2s of last panel scroll,
    /// or while actively dragging the panel scrollbar).
    pub fn panel_scrollbar_visible(&self) -> bool {
        if self.scrollbar_drag == Some(ScrollbarDragTarget::Panel) {
            return true;
        }
        match self.panel_scroll_activity {
            Some(when) => when.elapsed() < std::time::Duration::from_secs(2),
            None => false,
        }
    }

    pub fn record_graph_hscroll_activity(&mut self) {
        self.graph_hscroll_activity = Some(Instant::now());
    }

    #[allow(dead_code)]
    pub fn record_panel_hscroll_activity(&mut self) {
        self.panel_hscroll_activity = Some(Instant::now());
    }

    pub fn graph_hscrollbar_visible(&self) -> bool {
        if self.scrollbar_drag == Some(ScrollbarDragTarget::GraphHorizontal) {
            return true;
        }
        match self.graph_hscroll_activity {
            Some(when) => when.elapsed() < std::time::Duration::from_secs(2),
            None => false,
        }
    }

    #[allow(dead_code)]
    pub fn panel_hscrollbar_visible(&self) -> bool {
        if self.scrollbar_drag == Some(ScrollbarDragTarget::PanelHorizontal) {
            return true;
        }
        match self.panel_hscroll_activity {
            Some(when) => when.elapsed() < std::time::Duration::from_secs(2),
            None => false,
        }
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
            self.log_pane
                .rendered_lines
                .push("(no log entries)".to_string());
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
                    self.log_pane
                        .rendered_lines
                        .push(format!("[{}] {}", time_str, entry.message));
                }
            }
        }

        // If auto-tail is on, scroll to bottom so newest entries are visible.
        // Use usize::MAX — the render function clamps to the actual wrapped line count.
        if self.log_pane.auto_tail {
            self.log_pane.scroll = usize::MAX;
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
            .total_wrapped_lines
            .saturating_sub(self.log_pane.viewport_height);
        self.log_pane.scroll = (self.log_pane.scroll + amount).min(max_scroll);
        // If we reached the bottom, resume auto-tail.
        if self.log_pane.scroll >= max_scroll {
            self.log_pane.auto_tail = true;
        }
    }

    /// Scroll log pane to the very top.
    pub fn log_scroll_to_top(&mut self) {
        self.log_pane.scroll = 0;
        self.log_pane.auto_tail = false;
    }

    /// Scroll log pane to the very bottom.
    pub fn log_scroll_to_bottom(&mut self) {
        let max_scroll = self
            .log_pane
            .total_wrapped_lines
            .saturating_sub(self.log_pane.viewport_height);
        self.log_pane.scroll = max_scroll;
        self.log_pane.auto_tail = true;
    }

    /// Toggle log pane JSON mode.
    pub fn toggle_log_json(&mut self) {
        self.log_pane.json_mode = !self.log_pane.json_mode;
        self.invalidate_log_pane();
    }

    // ── Coordinator log (panel 7) ──

    /// Toggle coordinator log view: switch to CoordLog tab in right panel.
    pub fn toggle_coord_log(&mut self) {
        if self.right_panel_tab == RightPanelTab::CoordLog && self.right_panel_visible {
            self.right_panel_visible = false;
            self.focused_panel = FocusedPanel::Graph;
        } else {
            self.right_panel_visible = true;
            self.right_panel_tab = RightPanelTab::CoordLog;
            self.load_coord_log();
        }
    }

    /// Load coordinator activity log from daemon.log (incremental).
    pub fn load_coord_log(&mut self) {
        use std::io::{Seek, SeekFrom};
        let log_path = self.workgraph_dir.join("service").join("daemon.log");
        let file = match std::fs::File::open(&log_path) {
            Ok(f) => f,
            Err(_) => {
                if !self.coord_log.rendered_lines.is_empty() {
                    self.coord_log.rendered_lines.clear();
                    self.coord_log.last_offset = 0;
                }
                return;
            }
        };
        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if file_len < self.coord_log.last_offset {
            self.coord_log.rendered_lines.clear();
            self.coord_log.last_offset = 0;
        }
        if file_len == self.coord_log.last_offset {
            return;
        }
        let mut reader = BufReader::new(file);
        if self.coord_log.last_offset > 0
            && reader
                .seek(SeekFrom::Start(self.coord_log.last_offset))
                .is_err()
        {
            return;
        }
        let mut new_lines = Vec::new();
        let mut buf = String::new();
        while reader.read_line(&mut buf).unwrap_or(0) > 0 {
            let line = buf.trim_end().to_string();
            if !line.is_empty() {
                new_lines.push(line);
            }
            buf.clear();
        }
        self.coord_log.last_offset = file_len;
        self.coord_log.rendered_lines.extend(new_lines);
        if self.coord_log.auto_tail {
            self.coord_log.scroll = usize::MAX;
        }
    }

    /// Scroll coordinator log up.
    pub fn coord_log_scroll_up(&mut self, amount: usize) {
        self.coord_log.scroll = self.coord_log.scroll.saturating_sub(amount);
        self.coord_log.auto_tail = false;
    }

    /// Scroll coordinator log down.
    pub fn coord_log_scroll_down(&mut self, amount: usize) {
        let max_scroll = self
            .coord_log
            .total_wrapped_lines
            .saturating_sub(self.coord_log.viewport_height);
        self.coord_log.scroll = (self.coord_log.scroll + amount).min(max_scroll);
        if self.coord_log.scroll >= max_scroll {
            self.coord_log.auto_tail = true;
        }
    }

    /// Scroll coordinator log to top.
    pub fn coord_log_scroll_to_top(&mut self) {
        self.coord_log.scroll = 0;
        self.coord_log.auto_tail = false;
    }

    /// Scroll coordinator log to bottom.
    pub fn coord_log_scroll_to_bottom(&mut self) {
        let max_scroll = self
            .coord_log
            .total_wrapped_lines
            .saturating_sub(self.coord_log.viewport_height);
        self.coord_log.scroll = max_scroll;
        self.coord_log.auto_tail = true;
    }

    // ── Messages panel (panel 3) ──

    /// Load messages for the currently selected task into the messages panel.
    pub fn load_messages_panel(&mut self) {
        let task_id = match self.selected_task_id() {
            Some(id) => id.to_string(),
            None => {
                self.messages_panel.rendered_lines.clear();
                self.messages_panel.entries.clear();
                self.messages_panel.summary = MessageSummary::default();
                self.messages_panel.task_id = None;
                return;
            }
        };

        // Skip reload if already loaded for this task.
        if self.messages_panel.task_id.as_deref() == Some(&task_id) {
            return;
        }

        self.messages_panel.rendered_lines.clear();
        self.messages_panel.entries.clear();
        self.messages_panel.summary = MessageSummary::default();

        // Look up the assigned agent for direction detection.
        let assigned_agent = load_graph(self.workgraph_dir.join("graph.jsonl"))
            .ok()
            .and_then(|g| {
                g.tasks()
                    .find(|t| t.id == task_id)
                    .and_then(|t| t.assigned.clone())
            });

        match workgraph::messages::list_messages(&self.workgraph_dir, &task_id) {
            Ok(msgs) if msgs.is_empty() => {
                self.messages_panel
                    .rendered_lines
                    .push("(no messages)".to_string());
            }
            Ok(msgs) => {
                let now = chrono::Utc::now();
                let mut incoming = 0usize;
                let mut outgoing = 0usize;
                let mut last_incoming_idx: Option<usize> = None;
                let mut last_outgoing_idx: Option<usize> = None;
                let mut unanswered_incoming: Vec<usize> = Vec::new();

                for (i, msg) in msgs.iter().enumerate() {
                    let time_str = format_relative_time(&msg.timestamp, &now);
                    let is_urgent = msg.priority == "urgent";

                    // Determine direction: message from the assigned agent = outgoing.
                    let is_from_agent = assigned_agent.as_ref().is_some_and(|a| msg.sender == *a);
                    let direction = if is_from_agent {
                        MessageDirection::Outgoing
                    } else {
                        MessageDirection::Incoming
                    };

                    // Map sender to display label.
                    let is_user_sender =
                        matches!(msg.sender.as_str(), "user" | "tui" | "coordinator");
                    let display_label = if is_user_sender {
                        "you".to_string()
                    } else if is_from_agent {
                        "agent".to_string()
                    } else {
                        msg.sender.clone()
                    };

                    match direction {
                        MessageDirection::Incoming => {
                            incoming += 1;
                            last_incoming_idx = Some(i);
                            unanswered_incoming.push(i);
                        }
                        MessageDirection::Outgoing => {
                            outgoing += 1;
                            last_outgoing_idx = Some(i);
                            unanswered_incoming.clear();
                        }
                    }

                    // Keep rendered_lines for compat.
                    let priority_tag = if is_urgent { " [!]" } else { "" };
                    self.messages_panel.rendered_lines.push(format!(
                        "[{}] {}{}: {}",
                        time_str, msg.sender, priority_tag, msg.body
                    ));

                    self.messages_panel.entries.push(MessageEntry {
                        sender: msg.sender.clone(),
                        display_label,
                        body: msg.body.clone(),
                        timestamp: time_str,
                        is_urgent,
                        direction,
                        delivery_status: msg.status.clone(),
                    });
                }

                // Compute summary.
                let responded = match (last_incoming_idx, last_outgoing_idx) {
                    (Some(li), Some(lo)) => lo > li,
                    _ => outgoing > 0 && incoming == 0,
                };
                self.messages_panel.summary = MessageSummary {
                    incoming,
                    outgoing,
                    responded,
                    unanswered: unanswered_incoming.len(),
                };
            }
            Err(_) => {
                self.messages_panel
                    .rendered_lines
                    .push("(error loading messages)".to_string());
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
            show_system_tasks: false,
            system_tasks_just_toggled: false,
            mouse_enabled: false,
            last_graph_area: Rect::default(),
            last_right_panel_area: Rect::default(),
            last_tab_bar_area: Rect::default(),
            last_right_content_area: Rect::default(),
            last_chat_input_area: Rect::default(),
            last_chat_message_area: Rect::default(),
            last_text_prompt_area: Rect::default(),
            last_file_tree_area: Rect::default(),
            last_file_preview_area: Rect::default(),
            config_entry_y_positions: Vec::new(),
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
            phase_annotations: viz.phase_annotations.clone(),
            cycle_set: HashSet::new(),
            hud_detail: None,
            hud_scroll: 0,
            hud_wrapped_line_count: 0,
            hud_detail_viewport_height: 0,
            detail_raw_json: false,
            detail_collapsed_sections: std::collections::HashSet::new(),
            detail_section_header_lines: Vec::new(),
            right_panel_visible: false,
            focused_panel: FocusedPanel::Graph,
            right_panel_tab: RightPanelTab::Detail,
            right_panel_percent: 35,
            hud_size: HudSize::Normal,
            layout_mode: LayoutMode::ThirdInspector,

            input_mode: InputMode::Normal,
            needs_center_on_selected: false,
            needs_scroll_into_view: false,
            chat_input_dismissed: false,
            inspector_sub_focus: InspectorSubFocus::ChatHistory,
            task_form: None,
            text_prompt: TextPromptState {
                editor: new_emacs_editor(),
            },
            chat: ChatState::default(),
            agent_monitor: AgentMonitorState::default(),
            agent_streams: HashMap::new(),
            agency_lifecycle: None,
            log_pane: LogPaneState::default(),
            coord_log: CoordLogState::default(),
            messages_panel: MessagesPanelState::default(),
            cmd_rx: mpsc::channel().1,
            cmd_tx: mpsc::channel().0,
            notification: None,
            last_tab_press: None,
            sort_mode: SortMode::ReverseChronological,
            smart_follow_active: true,
            initial_load: false,
            splash_animations: HashMap::new(),
            task_snapshots: HashMap::new(),
            animation_mode: AnimationMode::Normal,
            message_name_threshold: 8,
            message_indent: 2,
            graph_scroll_activity: None,
            panel_scroll_activity: None,
            scrollbar_drag: None,
            last_graph_scrollbar_area: Rect::default(),
            last_panel_scrollbar_area: Rect::default(),
            graph_hscroll_activity: None,
            panel_hscroll_activity: None,
            last_graph_hscrollbar_area: Rect::default(),
            last_panel_hscrollbar_area: Rect::default(),
            has_keyboard_enhancement: false,
            editor_handler: create_editor_handler(),
            last_graph_mtime: None,
            last_refresh: Instant::now(),
            last_refresh_display: String::new(),
            refresh_interval: std::time::Duration::from_secs(3600),
            config_panel: ConfigPanelState::default(),
            file_browser: None,
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
    /// In full-inspector or off mode, focus stays locked to the visible content.
    pub fn toggle_panel_focus(&mut self) {
        match self.layout_mode {
            LayoutMode::FullInspector => {
                // Only the panel is visible; stay focused on it.
                self.focused_panel = FocusedPanel::RightPanel;
                return;
            }
            LayoutMode::Off => {
                // Only the graph is visible; stay focused on it.
                self.focused_panel = FocusedPanel::Graph;
                return;
            }
            LayoutMode::ThirdInspector
            | LayoutMode::HalfInspector
            | LayoutMode::TwoThirdsInspector => {}
        }
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
    /// If in a non-split layout mode, resets to ThirdInspector mode first.
    pub fn toggle_right_panel(&mut self) {
        if !self.layout_mode.has_graph() || !self.layout_mode.has_inspector() {
            // Reset to default split mode, then apply the toggle.
            self.layout_mode = LayoutMode::TwoThirdsInspector;
        }
        self.right_panel_visible = !self.right_panel_visible;
        if !self.right_panel_visible {
            self.focused_panel = FocusedPanel::Graph;
        }
    }

    /// Cycle HUD panel size between Normal (~1/3) and Expanded (~2/3).
    #[allow(dead_code)]
    pub fn cycle_hud_size(&mut self) {
        self.hud_size = self.hud_size.cycle();
        self.right_panel_percent = self.hud_size.side_percent();
    }

    /// Cycle layout mode forward: 1/3 → 1/2 → 2/3 → full → off → 1/3.
    pub fn cycle_layout_mode(&mut self) {
        self.apply_layout_mode(self.layout_mode.cycle());
    }

    /// Cycle layout mode in reverse: off → full → 2/3 → 1/2 → 1/3 → off.
    pub fn cycle_layout_mode_reverse(&mut self) {
        self.apply_layout_mode(self.layout_mode.cycle_reverse());
    }

    /// Apply a layout mode, updating panel visibility and focus.
    fn apply_layout_mode(&mut self, mode: LayoutMode) {
        self.layout_mode = mode;
        match mode {
            LayoutMode::ThirdInspector
            | LayoutMode::HalfInspector
            | LayoutMode::TwoThirdsInspector => {
                self.right_panel_visible = true;
                self.right_panel_percent = mode.panel_percent();
            }
            LayoutMode::FullInspector => {
                self.right_panel_visible = true;
                // Focus the right panel since it's the only visible content.
                self.focused_panel = FocusedPanel::RightPanel;
            }
            LayoutMode::Off => {
                self.right_panel_visible = false;
                self.focused_panel = FocusedPanel::Graph;
            }
        }
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
                        let err = result
                            .output
                            .lines()
                            .find(|l| !l.is_empty())
                            .unwrap_or("unknown");
                        format!("Error: {}", err)
                    };
                    self.notification = Some((msg, Instant::now()));
                }
                CommandEffect::RefreshAndNotify(msg) => {
                    self.force_refresh();
                    let msg = if result.success {
                        msg
                    } else {
                        let err = result
                            .output
                            .lines()
                            .find(|l| !l.is_empty())
                            .unwrap_or("unknown");
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
                            full_text: None,
                            attachments: vec![],
                        });
                        save_chat_history(&self.workgraph_dir, &self.chat.messages);
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
        if let Some((_, when)) = &self.notification
            && when.elapsed() > std::time::Duration::from_secs(3)
        {
            self.notification = None;
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
                        let started = chrono::DateTime::parse_from_rfc3339(&agent.started_at)
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

    /// Update live JSONL stream state for all active (Working) agents.
    /// Reads new lines from each agent's output.log since the last known offset.
    pub fn update_agent_streams(&mut self) {
        use std::io::{Read, Seek, SeekFrom};

        let agents_dir = self.workgraph_dir.join("agents");
        // Collect active agent IDs.
        let active_ids: Vec<String> = self
            .agent_monitor
            .agents
            .iter()
            .filter(|a| matches!(a.status, AgentStatus::Working))
            .map(|a| a.agent_id.clone())
            .collect();

        // Remove stale entries for agents no longer active.
        self.agent_streams.retain(|id, _| active_ids.contains(id));

        for agent_id in &active_ids {
            let log_path = agents_dir.join(agent_id).join("output.log");
            if !log_path.exists() {
                continue;
            }

            let info = self
                .agent_streams
                .entry(agent_id.clone())
                .or_insert_with(|| AgentStreamInfo {
                    file_offset: 0,
                    message_count: 0,
                    latest_snippet: None,
                    latest_is_tool: false,
                });

            // Open file and seek to last known position.
            let mut file = match std::fs::File::open(&log_path) {
                Ok(f) => f,
                Err(_) => continue,
            };

            let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if file_len <= info.file_offset {
                continue; // No new data.
            }

            if file.seek(SeekFrom::Start(info.file_offset)).is_err() {
                continue;
            }

            let mut new_data = String::new();
            if file.read_to_string(&mut new_data).is_err() {
                continue;
            }

            info.file_offset = file_len;

            // Parse each new JSONL line.
            for line in new_data.lines() {
                let line = line.trim();
                if line.is_empty() || !line.starts_with('{') {
                    continue;
                }
                let val: serde_json::Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                let msg_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

                match msg_type {
                    "assistant" => {
                        info.message_count += 1;
                        // Extract content from message.content array.
                        if let Some(content) = val
                            .get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_array())
                        {
                            // Process content blocks — last text or tool_use wins.
                            for block in content {
                                let block_type =
                                    block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                                match block_type {
                                    "text" => {
                                        if let Some(text) =
                                            block.get("text").and_then(|v| v.as_str())
                                        {
                                            let trimmed = text.trim();
                                            if !trimmed.is_empty() {
                                                // Take the last non-empty line as snippet.
                                                let snippet =
                                                    trimmed.lines().last().unwrap_or(trimmed);
                                                let snippet = if snippet.len() > 120 {
                                                    format!("{}…", &snippet[..snippet.floor_char_boundary(120)])
                                                } else {
                                                    snippet.to_string()
                                                };
                                                info.latest_snippet = Some(snippet);
                                                info.latest_is_tool = false;
                                            }
                                        }
                                    }
                                    "tool_use" => {
                                        let name = block
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?");
                                        // For Bash/Edit/Write, show a brief input summary.
                                        let detail = match name {
                                            "Bash" => block
                                                .get("input")
                                                .and_then(|i| i.get("command"))
                                                .and_then(|v| v.as_str())
                                                .map(|c| {
                                                    let c = c.trim();
                                                    if c.len() > 80 {
                                                        format!("{name}: {}…", &c[..c.floor_char_boundary(80)])
                                                    } else {
                                                        format!("{name}: {c}")
                                                    }
                                                }),
                                            "Read" | "Write" | "Edit" => block
                                                .get("input")
                                                .and_then(|i| i.get("file_path"))
                                                .and_then(|v| v.as_str())
                                                .map(|p| format!("{name}: {p}")),
                                            "Grep" => block
                                                .get("input")
                                                .and_then(|i| i.get("pattern"))
                                                .and_then(|v| v.as_str())
                                                .map(|p| format!("{name}: {p}")),
                                            "Glob" => block
                                                .get("input")
                                                .and_then(|i| i.get("pattern"))
                                                .and_then(|v| v.as_str())
                                                .map(|p| format!("{name}: {p}")),
                                            _ => None,
                                        };
                                        info.latest_snippet =
                                            Some(detail.unwrap_or_else(|| name.to_string()));
                                        info.latest_is_tool = true;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    "user" | "result" => {
                        info.message_count += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Load the agency lifecycle (assign → execute → evaluate) for the currently selected task.
    pub fn load_agency_lifecycle(&mut self) {
        let task_id = match self.selected_task_id() {
            Some(id) => id.to_string(),
            None => {
                self.agency_lifecycle = None;
                return;
            }
        };

        // Skip reload if already loaded for this task.
        if let Some(ref lc) = self.agency_lifecycle
            && lc.task_id == task_id
        {
            return;
        }

        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = match load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => {
                self.agency_lifecycle = None;
                return;
            }
        };

        let task = match graph.tasks().find(|t| t.id == task_id) {
            Some(t) => t.clone(),
            None => {
                self.agency_lifecycle = None;
                return;
            }
        };

        let agents_dir = self.workgraph_dir.join("agents");

        // Helper: build a LifecyclePhase from a task
        let wg_dir = self.workgraph_dir.clone();
        let build_phase = |t: &workgraph::graph::Task, label: &'static str| -> LifecyclePhase {
            let usage = t
                .token_usage
                .clone()
                .or_else(|| {
                    let agent_id = t.assigned.as_deref()?;
                    let log_path = agents_dir.join(agent_id).join("output.log");
                    parse_token_usage_live(&log_path)
                })
                .or_else(|| {
                    // Fall back to archived output
                    let archive_base = wg_dir.join("log").join("agents").join(&t.id);
                    if !archive_base.exists() {
                        return None;
                    }
                    let mut entries: Vec<_> = std::fs::read_dir(&archive_base)
                        .ok()?
                        .filter_map(|e| e.ok())
                        .filter(|e| e.file_type().ok().is_some_and(|ft| ft.is_dir()))
                        .collect();
                    entries.sort_by_key(|b| std::cmp::Reverse(b.file_name()));
                    for entry in entries {
                        let candidate = entry.path().join("output.txt");
                        if candidate.exists()
                            && let Some(u) = parse_token_usage_live(&candidate)
                        {
                            return Some(u);
                        }
                    }
                    None
                });
            let runtime_secs = match (&t.started_at, &t.completed_at) {
                (Some(s), Some(e)) => {
                    if let (Ok(start), Ok(end)) = (
                        chrono::DateTime::parse_from_rfc3339(s),
                        chrono::DateTime::parse_from_rfc3339(e),
                    ) {
                        Some((end - start).num_seconds())
                    } else {
                        None
                    }
                }
                (Some(s), None) if t.status == Status::InProgress => {
                    chrono::DateTime::parse_from_rfc3339(s).ok().map(|start| {
                        (chrono::Utc::now() - start.with_timezone(&chrono::Utc)).num_seconds()
                    })
                }
                _ => None,
            };
            LifecyclePhase {
                task_id: t.id.clone(),
                label,
                status: t.status,
                agent_id: t.assigned.clone(),
                token_usage: usage,
                runtime_secs,
                eval_score: None,
                eval_notes: None,
            }
        };

        // Assignment phase
        let assign_task_id = format!(".assign-{}", task_id);
        let legacy_assign_id = format!("assign-{}", task_id);
        let assignment = graph
            .tasks()
            .find(|t| t.id == assign_task_id || t.id == legacy_assign_id)
            .map(|t| build_phase(t, "Assignment"));

        // Execution phase (the task itself)
        let execution = Some(build_phase(&task, "Execution"));

        // Evaluation phase
        let eval_task_id = format!(".evaluate-{}", task_id);
        let legacy_eval_id = format!("evaluate-{}", task_id);
        let evaluation = graph.tasks().find(|t| t.id == eval_task_id || t.id == legacy_eval_id).map(|t| {
            let mut phase = build_phase(t, "Evaluation");

            // Load evaluation results from agency/evaluations/
            let evals_dir = self.workgraph_dir.join("agency").join("evaluations");
            if evals_dir.exists() {
                let prefix = format!("eval-{}-", task_id);
                if let Ok(entries) = std::fs::read_dir(&evals_dir) {
                    let mut eval_files: Vec<_> = entries
                        .filter_map(|e| e.ok())
                        .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
                        .collect();
                    eval_files.sort_by_key(|b| std::cmp::Reverse(b.file_name()));
                    if let Some(entry) = eval_files.first()
                        && let Ok(content) = std::fs::read_to_string(entry.path())
                        && let Ok(eval) = serde_json::from_str::<serde_json::Value>(&content)
                    {
                        phase.eval_score = eval.get("score").and_then(|v| v.as_f64());
                        phase.eval_notes = eval
                            .get("notes")
                            .and_then(|v| v.as_str())
                            .map(|s| s.lines().take(5).collect::<Vec<_>>().join("\n"));
                    }
                }
            }

            phase
        });

        self.agency_lifecycle = Some(AgencyLifecycle {
            task_id,
            assignment,
            execution,
            evaluation,
        });
    }

    /// Invalidate the agency lifecycle cache so it reloads on next render.
    pub fn invalidate_agency_lifecycle(&mut self) {
        self.agency_lifecycle = None;
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

    /// Load chat history on startup.
    /// Tries the persisted chat-history.json first, then falls back to inbox/outbox.
    pub fn load_chat_history(&mut self) {
        let persisted = load_persisted_chat_history(&self.workgraph_dir);
        if !persisted.is_empty() {
            self.chat.messages = persisted;
        } else {
            // Fall back to inbox/outbox (e.g. first run after upgrade).
            let history = workgraph::chat::read_history(&self.workgraph_dir).unwrap_or_default();

            self.chat.messages.clear();
            for msg in &history {
                let role = match msg.role.as_str() {
                    "user" => ChatRole::User,
                    "coordinator" => ChatRole::Coordinator,
                    _ => ChatRole::System,
                };
                let att_names: Vec<String> = msg
                    .attachments
                    .iter()
                    .map(|a| {
                        std::path::Path::new(&a.path)
                            .file_name()
                            .and_then(|f| f.to_str())
                            .unwrap_or(&a.path)
                            .to_string()
                    })
                    .collect();
                self.chat.messages.push(ChatMessage {
                    role,
                    text: msg.content.clone(),
                    full_text: msg.full_response.clone(),
                    attachments: att_names,
                });
            }

            // Persist the loaded history so next restart uses the file.
            if !self.chat.messages.is_empty() {
                save_chat_history(&self.workgraph_dir, &self.chat.messages);
            }
        }

        // Set outbox cursor to latest outbox message ID so we don't re-display old messages.
        if let Ok(msgs) = workgraph::chat::read_outbox_since(&self.workgraph_dir, 0) {
            self.chat.outbox_cursor = msgs.last().map(|m| m.id).unwrap_or(0);
        }
    }

    /// Poll for new coordinator responses in the outbox.
    /// Called during refresh ticks.
    pub fn poll_chat_messages(&mut self) {
        let new_msgs = match workgraph::chat::read_outbox_since(
            &self.workgraph_dir,
            self.chat.outbox_cursor,
        ) {
            Ok(msgs) => msgs,
            Err(_) => return,
        };

        if new_msgs.is_empty() {
            return;
        }

        for msg in &new_msgs {
            let att_names: Vec<String> = msg
                .attachments
                .iter()
                .map(|a| {
                    std::path::Path::new(&a.path)
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or(&a.path)
                        .to_string()
                })
                .collect();
            self.chat.messages.push(ChatMessage {
                role: ChatRole::Coordinator,
                text: msg.content.clone(),
                full_text: msg.full_response.clone(),
                attachments: att_names,
            });
        }

        // Persist updated chat history.
        save_chat_history(&self.workgraph_dir, &self.chat.messages);

        // Update cursor to latest message.
        self.chat.outbox_cursor = new_msgs
            .last()
            .map(|m| m.id)
            .unwrap_or(self.chat.outbox_cursor);

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
        let has_attachments = !self.chat.pending_attachments.is_empty();
        if text.trim().is_empty() && !has_attachments {
            return;
        }

        // Generate a request ID for correlating the response.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let request_id = format!("tui-{}-{}", now.as_millis(), now.subsec_nanos() % 100_000);

        // Collect attachment display names for the local message.
        let att_names: Vec<String> = self
            .chat
            .pending_attachments
            .iter()
            .map(|a| a.filename.clone())
            .collect();

        // Add user message to display immediately.
        self.chat.messages.push(ChatMessage {
            role: ChatRole::User,
            text: text.clone(),
            full_text: None,
            attachments: att_names,
        });

        // Persist updated chat history.
        save_chat_history(&self.workgraph_dir, &self.chat.messages);

        // Reset scroll to bottom.
        self.chat.scroll = 0;

        // Mark as awaiting response.
        self.chat.awaiting_response = true;
        self.chat.last_request_id = Some(request_id.clone());

        // Build `wg chat` command args, including --attachment flags.
        let mut args = vec!["chat".to_string(), text];
        for att in &self.chat.pending_attachments {
            args.push("--attachment".to_string());
            args.push(att.stored_path.clone());
        }

        // Clear pending attachments after sending.
        self.chat.pending_attachments.clear();

        // Send via `wg chat` command in background.
        self.exec_command(args, CommandEffect::ChatResponse(request_id));
    }

    /// Attempt to attach a file at the given path to the pending chat message.
    /// Validates and copies to .workgraph/attachments/.
    pub fn attach_file(&mut self, path_str: &str) {
        let source = std::path::Path::new(path_str.trim());
        match workgraph::chat::store_attachment(&self.workgraph_dir, source) {
            Ok(att) => {
                let filename = std::path::Path::new(&att.path)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or(&att.path)
                    .to_string();
                self.chat.pending_attachments.push(PendingAttachment {
                    filename: source
                        .file_name()
                        .and_then(|f| f.to_str())
                        .unwrap_or(path_str)
                        .to_string(),
                    stored_path: att.path,
                    mime_type: att.mime_type,
                    size_bytes: att.size_bytes,
                });
                self.notification =
                    Some((format!("Attached: {}", filename), std::time::Instant::now()));
            }
            Err(e) => {
                self.notification =
                    Some((format!("Attach failed: {}", e), std::time::Instant::now()));
            }
        }
    }

    /// Try to paste an image from the system clipboard.
    /// Returns `true` if an image was found and attached, `false` if no image
    /// (caller should fall through to text paste).
    pub fn try_paste_clipboard_image(&mut self) -> bool {
        match clipboard_grab_image(&self.workgraph_dir) {
            Ok(Some(att)) => {
                let filename = std::path::Path::new(&att.path)
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or(&att.path)
                    .to_string();
                self.chat.pending_attachments.push(PendingAttachment {
                    filename: filename.clone(),
                    stored_path: att.path,
                    mime_type: att.mime_type,
                    size_bytes: att.size_bytes,
                });
                self.notification = Some((
                    format!("Image pasted: {}", filename),
                    std::time::Instant::now(),
                ));
                true
            }
            Ok(None) => false, // no image on clipboard — fall through to text paste
            Err(e) => {
                self.notification =
                    Some((format!("Clipboard error: {}", e), std::time::Instant::now()));
                false // fall through to text paste on error
            }
        }
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
            for tag in form
                .tags
                .split(',')
                .map(|t| t.trim())
                .filter(|t| !t.is_empty())
            {
                args.push("--tag".to_string());
                args.push(tag.to_string());
            }
        }

        self.exec_command(
            args,
            CommandEffect::RefreshAndNotify("Task created".to_string()),
        );
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
                self.notification = Some(("No task selected".to_string(), Instant::now()));
                return;
            }
        };

        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = match load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => {
                self.notification = Some(("Failed to load graph".to_string(), Instant::now()));
                return;
            }
        };

        let agent_id = match graph.tasks().find(|t| t.id == task_id) {
            Some(task) => match &task.assigned {
                Some(id) => id.clone(),
                None => {
                    self.notification =
                        Some((format!("No active agent on '{}'", task_id), Instant::now()));
                    return;
                }
            },
            None => {
                self.notification = Some((format!("Task '{}' not found", task_id), Instant::now()));
                return;
            }
        };

        self.exec_command(
            vec!["kill".to_string(), agent_id.clone()],
            CommandEffect::RefreshAndNotify(format!("Killed {} on task '{}'", agent_id, task_id)),
        );
    }

    // ── Config panel ──

    /// Load configuration from disk and populate config panel entries.
    pub fn load_config_panel(&mut self) {
        let config = Config::load_or_default(&self.workgraph_dir);
        let model_choices = load_model_choices(&self.workgraph_dir);
        let mut entries = Vec::new();

        // ── 1. LLM Endpoints ──
        for (i, ep) in config.llm_endpoints.endpoints.iter().enumerate() {
            let status_icon = if ep.is_default { "✓ " } else { "  " };
            entries.push(ConfigEntry {
                key: format!("endpoint.{}.name", i),
                label: format!("{}{}  ({})", status_icon, ep.name, ep.provider),
                value: ep.url.clone().unwrap_or_else(|| {
                    workgraph::config::EndpointConfig::default_url_for_provider(&ep.provider)
                        .to_string()
                }),
                edit_kind: ConfigEditKind::TextInput,
                section: ConfigSection::Endpoints,
            });
            entries.push(ConfigEntry {
                key: format!("endpoint.{}.model", i),
                label: "  Model".into(),
                value: ep.model.clone().unwrap_or_else(|| "(default)".into()),
                edit_kind: ConfigEditKind::TextInput,
                section: ConfigSection::Endpoints,
            });
            entries.push(ConfigEntry {
                key: format!("endpoint.{}.api_key", i),
                label: "  API Key".into(),
                value: ep.masked_key(),
                edit_kind: ConfigEditKind::SecretInput,
                section: ConfigSection::Endpoints,
            });
            entries.push(ConfigEntry {
                key: format!("endpoint.{}.is_default", i),
                label: "  Set as default".into(),
                value: if ep.is_default {
                    "on".into()
                } else {
                    "off".into()
                },
                edit_kind: ConfigEditKind::Toggle,
                section: ConfigSection::Endpoints,
            });
            entries.push(ConfigEntry {
                key: format!("endpoint.{}.remove", i),
                label: "  Remove endpoint".into(),
                value: "▸".into(),
                edit_kind: ConfigEditKind::Toggle,
                section: ConfigSection::Endpoints,
            });
        }
        entries.push(ConfigEntry {
            key: "endpoint.add".into(),
            label: "+ Add endpoint".into(),
            value: String::new(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Endpoints,
        });

        // ── 2. API Keys (from environment) ──
        let mask_env = |var: &str| -> String {
            match std::env::var(var).ok().filter(|k| !k.is_empty()) {
                Some(key) if key.len() > 8 => {
                    format!("{}****...{}", &key[..key.floor_char_boundary(3)], &key[key.ceil_char_boundary(key.len() - 4)..])
                }
                Some(_) => "****".into(),
                None => "(not set)".into(),
            }
        };
        entries.push(ConfigEntry {
            key: "apikey.anthropic".into(),
            label: "Anthropic".into(),
            value: mask_env("ANTHROPIC_API_KEY"),
            edit_kind: ConfigEditKind::SecretInput,
            section: ConfigSection::ApiKeys,
        });
        entries.push(ConfigEntry {
            key: "apikey.openai".into(),
            label: "OpenAI".into(),
            value: mask_env("OPENAI_API_KEY"),
            edit_kind: ConfigEditKind::SecretInput,
            section: ConfigSection::ApiKeys,
        });
        entries.push(ConfigEntry {
            key: "apikey.openrouter".into(),
            label: "OpenRouter".into(),
            value: mask_env("OPENROUTER_API_KEY"),
            edit_kind: ConfigEditKind::SecretInput,
            section: ConfigSection::ApiKeys,
        });

        // ── 3. Service Settings ──
        {
            use crate::commands::service::{ServiceState, is_service_alive};
            let ss = ServiceState::load(&self.workgraph_dir).ok().flatten();
            self.config_panel.service_running =
                ss.as_ref().is_some_and(|s| is_service_alive(s.pid));
            self.config_panel.service_pid = ss.as_ref().map(|s| s.pid);
        }
        entries.push(ConfigEntry {
            key: "coordinator.max_agents".into(),
            label: "Max agents".into(),
            value: config.coordinator.max_agents.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Service,
        });
        entries.push(ConfigEntry {
            key: "coordinator.poll_interval".into(),
            label: "Poll interval (s)".into(),
            value: config.coordinator.poll_interval.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Service,
        });
        entries.push(ConfigEntry {
            key: "coordinator.executor".into(),
            label: "Executor".into(),
            value: config.coordinator.executor.clone(),
            edit_kind: ConfigEditKind::Choice(vec![
                "claude".into(),
                "amplifier".into(),
                "opencode".into(),
                "codex".into(),
                "shell".into(),
            ]),
            section: ConfigSection::Service,
        });
        entries.push(ConfigEntry {
            key: "coordinator.model".into(),
            label: "Model".into(),
            value: config
                .coordinator
                .model
                .clone()
                .unwrap_or_else(|| config.agent.model.clone()),
            edit_kind: ConfigEditKind::Choice(model_choices.clone()),
            section: ConfigSection::Service,
        });
        entries.push(ConfigEntry {
            key: "coordinator.agent_timeout".into(),
            label: "Agent timeout".into(),
            value: config.coordinator.agent_timeout.clone(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Service,
        });
        entries.push(ConfigEntry {
            key: "coordinator.settling_delay_ms".into(),
            label: "Settling delay (ms)".into(),
            value: config.coordinator.settling_delay_ms.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Service,
        });

        // ── 4. TUI Settings ──
        entries.push(ConfigEntry {
            key: "tui.mouse_mode".into(),
            label: "Mouse mode".into(),
            value: match config.tui.mouse_mode {
                Some(true) => "on".into(),
                Some(false) => "off".into(),
                None => "auto".into(),
            },
            edit_kind: ConfigEditKind::Choice(vec!["auto".into(), "on".into(), "off".into()]),
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "viz.animations".into(),
            label: "Animation speed".into(),
            value: config.viz.animations.clone(),
            edit_kind: ConfigEditKind::Choice(vec![
                "normal".into(),
                "fast".into(),
                "slow".into(),
                "reduced".into(),
                "off".into(),
            ]),
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.default_layout".into(),
            label: "Default layout".into(),
            value: config.tui.default_layout.clone(),
            edit_kind: ConfigEditKind::Choice(vec![
                "auto".into(),
                "horizontal".into(),
                "vertical".into(),
            ]),
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.default_inspector_size".into(),
            label: "Default inspector size".into(),
            value: config.tui.default_inspector_size.clone(),
            edit_kind: ConfigEditKind::Choice(vec![
                "1/3".into(),
                "1/2".into(),
                "2/3".into(),
                "full".into(),
            ]),
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.color_theme".into(),
            label: "Color theme".into(),
            value: config.tui.color_theme.clone(),
            edit_kind: ConfigEditKind::Choice(vec!["dark".into(), "light".into()]),
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.timestamp_format".into(),
            label: "Timestamp format".into(),
            value: config.tui.timestamp_format.clone(),
            edit_kind: ConfigEditKind::Choice(vec![
                "relative".into(),
                "iso".into(),
                "local".into(),
                "off".into(),
            ]),
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.show_token_counts".into(),
            label: "Token counts".into(),
            value: if config.tui.show_token_counts {
                "on".into()
            } else {
                "off".into()
            },
            edit_kind: ConfigEditKind::Toggle,
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "viz.edge_color".into(),
            label: "Edge color".into(),
            value: config.viz.edge_color.clone(),
            edit_kind: ConfigEditKind::Choice(vec!["gray".into(), "white".into(), "mixed".into()]),
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.message_name_threshold".into(),
            label: "Name threshold".into(),
            value: config.tui.message_name_threshold.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.message_indent".into(),
            label: "Message indent".into(),
            value: config.tui.message_indent.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::TuiSettings,
        });

        // ── 5. Agent Defaults ──
        entries.push(ConfigEntry {
            key: "agent.heartbeat_timeout".into(),
            label: "Heartbeat timeout (min)".into(),
            value: config.agent.heartbeat_timeout.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::AgentDefaults,
        });
        entries.push(ConfigEntry {
            key: "agent.executor".into(),
            label: "Default executor".into(),
            value: config.agent.executor.clone(),
            edit_kind: ConfigEditKind::Choice(vec![
                "claude".into(),
                "amplifier".into(),
                "opencode".into(),
                "codex".into(),
                "shell".into(),
            ]),
            section: ConfigSection::AgentDefaults,
        });
        entries.push(ConfigEntry {
            key: "agent.model".into(),
            label: "Default model".into(),
            value: config.agent.model.clone(),
            edit_kind: ConfigEditKind::Choice(model_choices.clone()),
            section: ConfigSection::AgentDefaults,
        });
        // ── 6. Agency ──
        entries.push(ConfigEntry {
            key: "agency.auto_assign".into(),
            label: "Auto-assign".into(),
            value: if config.agency.auto_assign {
                "on".into()
            } else {
                "off".into()
            },
            edit_kind: ConfigEditKind::Toggle,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.auto_evaluate".into(),
            label: "Auto-evaluate".into(),
            value: if config.agency.auto_evaluate {
                "on".into()
            } else {
                "off".into()
            },
            edit_kind: ConfigEditKind::Toggle,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.auto_triage".into(),
            label: "Auto-triage".into(),
            value: if config.agency.auto_triage {
                "on".into()
            } else {
                "off".into()
            },
            edit_kind: ConfigEditKind::Toggle,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.auto_create".into(),
            label: "Auto-create".into(),
            value: if config.agency.auto_create {
                "on".into()
            } else {
                "off".into()
            },
            edit_kind: ConfigEditKind::Toggle,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.run_mode".into(),
            label: "Run mode".into(),
            value: format!("{:.1}", config.agency.run_mode),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.assigner_model".into(),
            label: "Assigner model".into(),
            value: config
                .agency
                .assigner_model
                .clone()
                .unwrap_or_else(|| "(default)".into()),
            edit_kind: ConfigEditKind::Choice(vec![
                "(default)".into(),
                "opus".into(),
                "sonnet".into(),
                "haiku".into(),
            ]),
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.evaluator_model".into(),
            label: "Evaluator model".into(),
            value: config
                .agency
                .evaluator_model
                .clone()
                .unwrap_or_else(|| "(default)".into()),
            edit_kind: ConfigEditKind::Choice(vec![
                "(default)".into(),
                "opus".into(),
                "sonnet".into(),
                "haiku".into(),
            ]),
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.evolver_model".into(),
            label: "Evolver model".into(),
            value: config
                .agency
                .evolver_model
                .clone()
                .unwrap_or_else(|| "(default)".into()),
            edit_kind: ConfigEditKind::Choice(vec![
                "(default)".into(),
                "opus".into(),
                "sonnet".into(),
                "haiku".into(),
            ]),
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.creator_model".into(),
            label: "Creator model".into(),
            value: config
                .agency
                .creator_model
                .clone()
                .unwrap_or_else(|| "(default)".into()),
            edit_kind: ConfigEditKind::Choice(vec![
                "(default)".into(),
                "opus".into(),
                "sonnet".into(),
                "haiku".into(),
            ]),
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.triage_model".into(),
            label: "Triage model".into(),
            value: config
                .agency
                .triage_model
                .clone()
                .unwrap_or_else(|| "(default)".into()),
            edit_kind: ConfigEditKind::Choice(vec![
                "(default)".into(),
                "opus".into(),
                "sonnet".into(),
                "haiku".into(),
            ]),
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.assigner_agent".into(),
            label: "Assigner agent".into(),
            value: config
                .agency
                .assigner_agent
                .clone()
                .unwrap_or_else(|| "(none)".into()),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.evaluator_agent".into(),
            label: "Evaluator agent".into(),
            value: config
                .agency
                .evaluator_agent
                .clone()
                .unwrap_or_else(|| "(none)".into()),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.evolver_agent".into(),
            label: "Evolver agent".into(),
            value: config
                .agency
                .evolver_agent
                .clone()
                .unwrap_or_else(|| "(none)".into()),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.creator_agent".into(),
            label: "Creator agent".into(),
            value: config
                .agency
                .creator_agent
                .clone()
                .unwrap_or_else(|| "(none)".into()),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.auto_create_threshold".into(),
            label: "Auto-create threshold".into(),
            value: config.agency.auto_create_threshold.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.triage_timeout".into(),
            label: "Triage timeout (s)".into(),
            value: config
                .agency
                .triage_timeout
                .map(|t| t.to_string())
                .unwrap_or_else(|| "30".into()),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.triage_max_log_bytes".into(),
            label: "Triage max log bytes".into(),
            value: config
                .agency
                .triage_max_log_bytes
                .map(|b| b.to_string())
                .unwrap_or_else(|| "50000".into()),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.retention_heuristics".into(),
            label: "Retention heuristics".into(),
            value: config
                .agency
                .retention_heuristics
                .clone()
                .unwrap_or_else(|| "(not set)".into()),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });

        // ── 7. Guardrails ──
        entries.push(ConfigEntry {
            key: "guardrails.max_child_tasks_per_agent".into(),
            label: "Max subtasks/agent".into(),
            value: config.guardrails.max_child_tasks_per_agent.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Guardrails,
        });
        entries.push(ConfigEntry {
            key: "guardrails.max_task_depth".into(),
            label: "Max chain depth".into(),
            value: config.guardrails.max_task_depth.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Guardrails,
        });

        self.config_panel.entries = entries;
        if self.config_panel.selected >= self.config_panel.entries.len() {
            self.config_panel.selected = 0;
        }
    }

    /// Apply the current edit to the config and save to disk.
    pub fn save_config_entry(&mut self) {
        let idx = self.config_panel.selected;
        if idx >= self.config_panel.entries.len() {
            return;
        }

        let new_value = if self.config_panel.editing {
            match &self.config_panel.entries[idx].edit_kind {
                ConfigEditKind::TextInput | ConfigEditKind::SecretInput => {
                    self.config_panel.edit_buffer.clone()
                }
                ConfigEditKind::Choice(choices) => choices
                    .get(self.config_panel.choice_index)
                    .cloned()
                    .unwrap_or_default(),
                ConfigEditKind::Toggle => {
                    return;
                }
            }
        } else {
            return;
        };

        self.config_panel.entries[idx].value = new_value.clone();

        let mut config = Config::load_or_default(&self.workgraph_dir);
        let key = self.config_panel.entries[idx].key.clone();
        match key.as_str() {
            "coordinator.max_agents" => {
                if let Ok(v) = new_value.parse::<usize>() {
                    config.coordinator.max_agents = v;
                }
            }
            "coordinator.poll_interval" => {
                if let Ok(v) = new_value.parse::<u64>() {
                    config.coordinator.poll_interval = v;
                }
            }
            "coordinator.executor" => config.coordinator.executor = new_value,
            "coordinator.model" => config.coordinator.model = Some(new_value),
            "coordinator.agent_timeout" => config.coordinator.agent_timeout = new_value,
            "coordinator.settling_delay_ms" => {
                if let Ok(v) = new_value.parse::<u64>() {
                    config.coordinator.settling_delay_ms = v;
                }
            }
            "agent.heartbeat_timeout" => {
                if let Ok(v) = new_value.parse::<u64>() {
                    config.agent.heartbeat_timeout = v;
                }
            }
            "agent.executor" => config.agent.executor = new_value,
            "agent.model" => config.agent.model = new_value,
            "agency.auto_evaluate" => config.agency.auto_evaluate = new_value == "on",
            "agency.auto_assign" => config.agency.auto_assign = new_value == "on",
            "agency.auto_triage" => config.agency.auto_triage = new_value == "on",
            "agency.auto_create" => config.agency.auto_create = new_value == "on",
            "agency.run_mode" => {
                if let Ok(v) = new_value.parse::<f64>() {
                    config.agency.run_mode = v.clamp(0.0, 1.0);
                }
            }
            "agency.assigner_model" => {
                config.agency.assigner_model = if new_value == "(default)" {
                    None
                } else {
                    Some(new_value)
                };
            }
            "agency.evaluator_model" => {
                config.agency.evaluator_model = if new_value == "(default)" {
                    None
                } else {
                    Some(new_value)
                };
            }
            "agency.evolver_model" => {
                config.agency.evolver_model = if new_value == "(default)" {
                    None
                } else {
                    Some(new_value)
                };
            }
            "agency.creator_model" => {
                config.agency.creator_model = if new_value == "(default)" {
                    None
                } else {
                    Some(new_value)
                };
            }
            "agency.triage_model" => {
                config.agency.triage_model = if new_value == "(default)" {
                    None
                } else {
                    Some(new_value)
                };
            }
            "agency.assigner_agent" => {
                config.agency.assigner_agent = if new_value == "(none)" || new_value.is_empty() {
                    None
                } else {
                    Some(new_value)
                };
            }
            "agency.evaluator_agent" => {
                config.agency.evaluator_agent = if new_value == "(none)" || new_value.is_empty() {
                    None
                } else {
                    Some(new_value)
                };
            }
            "agency.evolver_agent" => {
                config.agency.evolver_agent = if new_value == "(none)" || new_value.is_empty() {
                    None
                } else {
                    Some(new_value)
                };
            }
            "agency.creator_agent" => {
                config.agency.creator_agent = if new_value == "(none)" || new_value.is_empty() {
                    None
                } else {
                    Some(new_value)
                };
            }
            "agency.auto_create_threshold" => {
                if let Ok(v) = new_value.parse::<u32>() {
                    config.agency.auto_create_threshold = v;
                }
            }
            "agency.triage_timeout" => {
                config.agency.triage_timeout = new_value.parse::<u64>().ok();
            }
            "agency.triage_max_log_bytes" => {
                config.agency.triage_max_log_bytes = new_value.parse::<usize>().ok();
            }
            "agency.retention_heuristics" => {
                config.agency.retention_heuristics =
                    if new_value == "(not set)" || new_value.is_empty() {
                        None
                    } else {
                        Some(new_value)
                    };
            }
            "viz.edge_color" => config.viz.edge_color = new_value,
            "viz.animations" => {
                config.viz.animations = new_value.clone();
                self.animation_mode = AnimationMode::from_config(&new_value);
            }
            "tui.mouse_mode" => {
                config.tui.mouse_mode = match new_value.as_str() {
                    "on" => Some(true),
                    "off" => Some(false),
                    _ => None,
                };
            }
            "tui.default_layout" => config.tui.default_layout = new_value,
            "tui.default_inspector_size" => config.tui.default_inspector_size = new_value,
            "tui.color_theme" => config.tui.color_theme = new_value,
            "tui.timestamp_format" => config.tui.timestamp_format = new_value,
            "tui.show_token_counts" => config.tui.show_token_counts = new_value == "on",
            "tui.message_name_threshold" => {
                if let Ok(v) = new_value.parse::<u16>() {
                    config.tui.message_name_threshold = v;
                    self.message_name_threshold = v;
                }
            }
            "tui.message_indent" => {
                if let Ok(v) = new_value.parse::<u16>() {
                    let clamped = v.min(8);
                    config.tui.message_indent = clamped;
                    self.message_indent = clamped;
                }
            }
            "guardrails.max_child_tasks_per_agent" => {
                if let Ok(v) = new_value.parse::<u32>() {
                    config.guardrails.max_child_tasks_per_agent = v;
                }
            }
            "guardrails.max_task_depth" => {
                if let Ok(v) = new_value.parse::<u32>() {
                    config.guardrails.max_task_depth = v;
                }
            }
            _ => {
                // Endpoint fields: endpoint.N.field
                if let Some(rest) = key.strip_prefix("endpoint.") {
                    let parts: Vec<&str> = rest.splitn(2, '.').collect();
                    if parts.len() == 2
                        && let Ok(ep_idx) = parts[0].parse::<usize>()
                        && ep_idx < config.llm_endpoints.endpoints.len()
                    {
                        match parts[1] {
                            "name" => config.llm_endpoints.endpoints[ep_idx].name = new_value,
                            "model" => {
                                config.llm_endpoints.endpoints[ep_idx].model = Some(new_value)
                            }
                            "api_key" => {
                                config.llm_endpoints.endpoints[ep_idx].api_key =
                                    Some(new_value.clone());
                                self.config_panel.entries[idx].value =
                                    config.llm_endpoints.endpoints[ep_idx].masked_key();
                            }
                            _ => config.llm_endpoints.endpoints[ep_idx].url = Some(new_value),
                        }
                    }
                }
                // API keys from env are read-only in the TUI
                if key.starts_with("apikey.") {
                    self.config_panel.editing = false;
                    return;
                }
            }
        }
        if config.save(&self.workgraph_dir).is_ok() {
            self.config_panel.save_notification = Some(Instant::now());
        }

        self.config_panel.editing = false;
    }

    /// Toggle a boolean config entry and save immediately.
    pub fn toggle_config_entry(&mut self) {
        let idx = self.config_panel.selected;
        if idx >= self.config_panel.entries.len() {
            return;
        }
        if !matches!(
            self.config_panel.entries[idx].edit_kind,
            ConfigEditKind::Toggle
        ) {
            return;
        }

        let key = self.config_panel.entries[idx].key.clone();

        // Handle endpoint removal
        if key.ends_with(".remove") {
            if let Some(rest) = key.strip_prefix("endpoint.")
                && let Some(idx_str) = rest.strip_suffix(".remove")
                && let Ok(ep_idx) = idx_str.parse::<usize>()
            {
                let mut config = Config::load_or_default(&self.workgraph_dir);
                if ep_idx < config.llm_endpoints.endpoints.len() {
                    config.llm_endpoints.endpoints.remove(ep_idx);
                    let _ = config.save(&self.workgraph_dir);
                    self.config_panel.save_notification = Some(Instant::now());
                    self.load_config_panel();
                }
            }
            return;
        }

        // Handle set-as-default
        if key.ends_with(".is_default") {
            if let Some(rest) = key.strip_prefix("endpoint.")
                && let Some(idx_str) = rest.strip_suffix(".is_default")
                && let Ok(ep_idx) = idx_str.parse::<usize>()
            {
                let mut config = Config::load_or_default(&self.workgraph_dir);
                for (i, ep) in config.llm_endpoints.endpoints.iter_mut().enumerate() {
                    ep.is_default = i == ep_idx;
                }
                let _ = config.save(&self.workgraph_dir);
                self.config_panel.save_notification = Some(Instant::now());
                self.load_config_panel();
            }
            return;
        }

        let new_val = if self.config_panel.entries[idx].value == "on" {
            "off"
        } else {
            "on"
        };
        self.config_panel.entries[idx].value = new_val.to_string();

        let mut config = Config::load_or_default(&self.workgraph_dir);
        match key.as_str() {
            "agency.auto_evaluate" => config.agency.auto_evaluate = new_val == "on",
            "agency.auto_assign" => config.agency.auto_assign = new_val == "on",
            "agency.auto_triage" => config.agency.auto_triage = new_val == "on",
            "agency.auto_create" => config.agency.auto_create = new_val == "on",
            "tui.show_token_counts" => config.tui.show_token_counts = new_val == "on",
            _ => {}
        }
        if config.save(&self.workgraph_dir).is_ok() {
            self.config_panel.save_notification = Some(Instant::now());
        }
    }

    /// Add a new endpoint from the new-endpoint form fields.
    pub fn add_endpoint(&mut self) {
        let fields = &self.config_panel.new_endpoint;
        if fields.name.trim().is_empty() {
            self.notification = Some(("Endpoint name is required".to_string(), Instant::now()));
            return;
        }
        let mut config = Config::load_or_default(&self.workgraph_dir);
        let provider = if fields.provider.is_empty() {
            "anthropic".to_string()
        } else {
            fields.provider.clone()
        };
        let is_first = config.llm_endpoints.endpoints.is_empty();
        config
            .llm_endpoints
            .endpoints
            .push(workgraph::config::EndpointConfig {
                name: fields.name.trim().to_string(),
                provider,
                url: if fields.url.is_empty() {
                    None
                } else {
                    Some(fields.url.clone())
                },
                model: if fields.model.is_empty() {
                    None
                } else {
                    Some(fields.model.clone())
                },
                api_key: if fields.api_key.is_empty() {
                    None
                } else {
                    Some(fields.api_key.clone())
                },
                is_default: is_first,
            });
        if config.save(&self.workgraph_dir).is_ok() {
            self.config_panel.save_notification = Some(Instant::now());
        }
        self.config_panel.adding_endpoint = false;
        self.config_panel.new_endpoint = NewEndpointFields::default();
        self.config_panel.new_endpoint_field = 0;
        self.load_config_panel();
    }

    /// Toggle collapse state for a config section.
    #[allow(dead_code)]
    pub fn toggle_config_section(&mut self, section: ConfigSection) {
        if self.config_panel.collapsed.contains(&section) {
            self.config_panel.collapsed.remove(&section);
        } else {
            self.config_panel.collapsed.insert(section);
        }
    }

    /// Get the section of the currently selected config entry.
    #[allow(dead_code)]
    pub fn selected_config_section(&self) -> Option<ConfigSection> {
        self.config_panel
            .entries
            .get(self.config_panel.selected)
            .map(|e| e.section)
    }

    /// Return entries filtered by collapsed state, with original indices.
    pub fn visible_config_entries(&self) -> Vec<(usize, &ConfigEntry)> {
        self.config_panel
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !self.config_panel.collapsed.contains(&e.section))
            .collect()
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Clipboard image detection
// ══════════════════════════════════════════════════════════════════════════════

/// Detect the clipboard environment and attempt to extract image data.
///
/// Returns `Ok(Some(attachment))` if an image was found and saved,
/// `Ok(None)` if the clipboard contains no image (caller should fall through to text paste),
/// or `Err` on unexpected failures.
///
/// Strategy (shell-out, no extra deps):
/// - **Linux X11**: `xclip -selection clipboard -t TARGETS -o` to check, then extract PNG
/// - **Linux Wayland**: `wl-paste --list-types` to check, then extract PNG
/// - **macOS**: `osascript` to check clipboard type, then extract via `pngpaste` or `osascript`
fn clipboard_grab_image(
    workgraph_dir: &std::path::Path,
) -> Result<Option<workgraph::chat::Attachment>> {
    // Detect environment
    if cfg!(target_os = "macos") {
        clipboard_grab_macos(workgraph_dir)
    } else if cfg!(target_os = "linux") {
        // Check Wayland first ($WAYLAND_DISPLAY), then X11 ($DISPLAY)
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            clipboard_grab_wayland(workgraph_dir)
        } else if std::env::var("DISPLAY").is_ok() {
            clipboard_grab_x11(workgraph_dir)
        } else {
            // Pure SSH / no display server — can't access clipboard
            Ok(None)
        }
    } else {
        // Unsupported platform
        Ok(None)
    }
}

/// Generate a temp file path for clipboard image extraction.
fn clipboard_temp_path(workgraph_dir: &std::path::Path) -> std::path::PathBuf {
    let now = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    workgraph_dir
        .join("attachments")
        .join(format!("clipboard-{}-{}.png", now, nanos % 100_000))
}

/// Linux X11: use xclip to detect and extract clipboard image.
fn clipboard_grab_x11(
    workgraph_dir: &std::path::Path,
) -> Result<Option<workgraph::chat::Attachment>> {
    // Check if xclip is available and clipboard has image data
    let targets = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
        .output();

    let targets = match targets {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Ok(None), // xclip not available or clipboard empty
    };

    // Look for image MIME types in the targets list
    let has_image = targets.lines().any(|line| {
        let t = line.trim();
        t == "image/png" || t == "image/jpeg" || t == "image/bmp"
    });

    if !has_image {
        return Ok(None);
    }

    // Extract the image data (prefer PNG)
    let mime = if targets.lines().any(|l| l.trim() == "image/png") {
        "image/png"
    } else if targets.lines().any(|l| l.trim() == "image/jpeg") {
        "image/jpeg"
    } else {
        "image/bmp"
    };

    let output = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", mime, "-o"])
        .output()?;

    if !output.status.success() || output.stdout.is_empty() {
        return Ok(None);
    }

    save_clipboard_image(workgraph_dir, &output.stdout)
}

/// Linux Wayland: use wl-paste to detect and extract clipboard image.
fn clipboard_grab_wayland(
    workgraph_dir: &std::path::Path,
) -> Result<Option<workgraph::chat::Attachment>> {
    // Check available MIME types
    let types = Command::new("wl-paste").arg("--list-types").output();

    let types = match types {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Ok(None), // wl-paste not available or clipboard empty
    };

    let has_image = types.lines().any(|line| {
        let t = line.trim();
        t == "image/png" || t == "image/jpeg" || t == "image/bmp"
    });

    if !has_image {
        return Ok(None);
    }

    // Extract as PNG (wl-paste can convert)
    let output = Command::new("wl-paste")
        .args(["--type", "image/png"])
        .output()?;

    if !output.status.success() || output.stdout.is_empty() {
        return Ok(None);
    }

    save_clipboard_image(workgraph_dir, &output.stdout)
}

/// macOS: use osascript/pngpaste to detect and extract clipboard image.
fn clipboard_grab_macos(
    workgraph_dir: &std::path::Path,
) -> Result<Option<workgraph::chat::Attachment>> {
    // Check clipboard type via osascript
    let check = Command::new("osascript")
        .args(["-e", "clipboard info"])
        .output();

    let info = match check {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Ok(None),
    };

    // Look for image types in clipboard info (e.g. «class PNGf», «class TIFF»)
    let has_image = info.contains("PNGf")
        || info.contains("TIFF")
        || info.contains("JPEG")
        || info.contains("public.png")
        || info.contains("public.tiff");

    if !has_image {
        return Ok(None);
    }

    // Try pngpaste first (cleaner, widely available via Homebrew)
    let tmp = clipboard_temp_path(workgraph_dir);
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let pngpaste_result = Command::new("pngpaste").arg(&tmp).output();

    if let Ok(o) = pngpaste_result
        && o.status.success()
        && tmp.exists()
    {
        let att = workgraph::chat::store_attachment(workgraph_dir, &tmp)?;
        // Clean up temp file (store_attachment copies it)
        let _ = std::fs::remove_file(&tmp);
        return Ok(Some(att));
    }

    // Fallback: use osascript to extract PNG data
    let script = format!(
        "set imgData to the clipboard as \u{ab}class PNGf\u{bb}\nset fp to open for access POSIX file \"{}\" with write permission\nwrite imgData to fp\nclose access fp",
        tmp.display()
    );

    let result = Command::new("osascript").args(["-e", &script]).output();

    match result {
        Ok(o) if o.status.success() && tmp.exists() => {
            let att = workgraph::chat::store_attachment(workgraph_dir, &tmp)?;
            let _ = std::fs::remove_file(&tmp);
            Ok(Some(att))
        }
        _ => {
            let _ = std::fs::remove_file(&tmp);
            Ok(None)
        }
    }
}

/// Save raw image bytes to a temp file, then store as an attachment.
fn save_clipboard_image(
    workgraph_dir: &std::path::Path,
    image_bytes: &[u8],
) -> Result<Option<workgraph::chat::Attachment>> {
    let tmp = clipboard_temp_path(workgraph_dir);
    if let Some(parent) = tmp.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp, image_bytes)?;
    let att = workgraph::chat::store_attachment(workgraph_dir, &tmp)?;
    // Clean up the temp file (store_attachment copies it with content-addressed name)
    let _ = std::fs::remove_file(&tmp);
    Ok(Some(att))
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
    entries.sort_by_key(|b| std::cmp::Reverse(b.file_name()));
    for entry in entries {
        let candidate = entry.path().join(filename);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Format an ISO 8601 timestamp for HUD display (shorter, local time).
/// Flatten a JSON value into human-readable key/value lines.
/// Strings are displayed directly, objects show "Key: Value", arrays are listed.
/// Nested objects recurse with increased indent.
fn flatten_json_to_lines(val: &serde_json::Value, indent: &str, lines: &mut Vec<String>) {
    match val {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                match value {
                    serde_json::Value::String(s) => {
                        // Capitalize the key nicely
                        let label = humanize_key(key);
                        for (i, line) in s.lines().enumerate() {
                            if i == 0 {
                                lines.push(format!("{}{}: {}", indent, label, line));
                            } else {
                                let continuation = " ".repeat(indent.len() + label.len() + 2);
                                lines.push(format!("{}{}", continuation, line));
                            }
                        }
                    }
                    serde_json::Value::Number(n) => {
                        lines.push(format!("{}{}: {}", indent, humanize_key(key), n));
                    }
                    serde_json::Value::Bool(b) => {
                        lines.push(format!("{}{}: {}", indent, humanize_key(key), b));
                    }
                    serde_json::Value::Null => {
                        lines.push(format!("{}{}: null", indent, humanize_key(key)));
                    }
                    serde_json::Value::Array(arr) => {
                        lines.push(format!("{}{}:", indent, humanize_key(key)));
                        let child_indent = format!("{}  ", indent);
                        for item in arr {
                            match item {
                                serde_json::Value::String(s) => {
                                    lines.push(format!("{}- {}", child_indent, s));
                                }
                                serde_json::Value::Object(_) => {
                                    flatten_json_to_lines(item, &child_indent, lines);
                                    lines.push(String::new());
                                }
                                other => {
                                    lines.push(format!("{}- {}", child_indent, other));
                                }
                            }
                        }
                    }
                    serde_json::Value::Object(_) => {
                        lines.push(format!("{}{}:", indent, humanize_key(key)));
                        let child_indent = format!("{}  ", indent);
                        flatten_json_to_lines(value, &child_indent, lines);
                    }
                }
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                flatten_json_to_lines(item, indent, lines);
                lines.push(String::new());
            }
        }
        serde_json::Value::String(s) => {
            lines.push(format!("{}{}", indent, s));
        }
        other => {
            lines.push(format!("{}{}", indent, other));
        }
    }
}

/// Convert a snake_case or camelCase JSON key to a human-readable label.
fn humanize_key(key: &str) -> String {
    // Replace underscores and split camelCase, then capitalize first letter.
    let mut result = String::new();
    let mut prev_lower = false;
    for (i, c) in key.chars().enumerate() {
        if c == '_' || c == '-' {
            result.push(' ');
            prev_lower = false;
        } else if c.is_uppercase() && prev_lower {
            result.push(' ');
            result.push(c.to_lowercase().next().unwrap_or(c));
            prev_lower = false;
        } else {
            if i == 0 {
                result.push(c.to_uppercase().next().unwrap_or(c));
            } else {
                result.push(c);
            }
            prev_lower = c.is_lowercase();
        }
    }
    result
}

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
        return dt.with_timezone(&chrono::Local).format("%H:%M").to_string();
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

    pub fn page_left(&mut self) {
        self.scroll_left(self.viewport_width / 2);
    }

    pub fn page_right(&mut self) {
        self.scroll_right(self.viewport_width / 2);
    }

    #[allow(dead_code)]
    pub fn go_leftmost(&mut self) {
        self.offset_x = 0;
    }

    #[allow(dead_code)]
    pub fn go_rightmost(&mut self) {
        self.offset_x = self.content_width.saturating_sub(self.viewport_width);
    }

    pub fn has_horizontal_overflow(&self) -> bool {
        self.content_width > self.viewport_width
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
            phase_annotations: HashMap::new(),
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
    fn hud_description_shows_full_content() {
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

        // Full description: all 15 lines should be present, no truncation
        assert_eq!(
            desc_lines.len(),
            15,
            "Description should show all 15 lines, got {}",
            desc_lines.len()
        );
        assert!(
            !desc_lines.iter().any(|l| l.contains("...")),
            "Full description should not show '...' truncation indicator"
        );
    }

    #[test]
    fn hud_section_collapse_toggle() {
        let mut graph = WorkGraph::new();
        let mut task = make_task_with_status("collapse-test", "Collapse Test", Status::Open);
        task.description = Some("Line 1\nLine 2\nLine 3".to_string());
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

        let mut app = build_app(&viz, "collapse-test", tmp.path());
        app.load_hud_detail();

        // Verify Description section exists
        assert!(app.detail_collapsed_sections.is_empty());

        // Scroll to the Description header and toggle
        let detail = app.hud_detail.as_ref().unwrap();
        let desc_idx = detail
            .rendered_lines
            .iter()
            .position(|l| l.contains("── Description ──"))
            .expect("should have description section");
        app.hud_scroll = desc_idx;
        let toggled = app.toggle_detail_section_at_scroll();
        assert_eq!(toggled, Some("Description".to_string()));
        assert!(app.detail_collapsed_sections.contains("Description"));

        // Toggle again to expand
        let toggled = app.toggle_detail_section_at_scroll();
        assert_eq!(toggled, Some("Description".to_string()));
        assert!(!app.detail_collapsed_sections.contains("Description"));
    }

    #[test]
    fn hud_section_toggle_by_name() {
        let mut graph = WorkGraph::new();
        let mut task = make_task_with_status("toggle-name", "Toggle Name", Status::Open);
        task.description = Some("Some content\nSecond line".to_string());
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

        let mut app = build_app(&viz, "toggle-name", tmp.path());
        app.load_hud_detail();

        // Toggle by name
        app.toggle_detail_section_by_name("Description");
        assert!(app.detail_collapsed_sections.contains("Description"));

        // Toggle again to expand
        app.toggle_detail_section_by_name("Description");
        assert!(!app.detail_collapsed_sections.contains("Description"));

        // State persists across task switches (same session)
        app.toggle_detail_section_by_name("Description");
        assert!(app.detail_collapsed_sections.contains("Description"));
        // Collapsed state stays even if we reload hud_detail
        app.load_hud_detail();
        assert!(app.detail_collapsed_sections.contains("Description"));
    }
}
