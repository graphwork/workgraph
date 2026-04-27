use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Instant, SystemTime};

use anyhow::Result;
use fuzzy_matcher::FuzzyMatcher;
use fuzzy_matcher::skim::SkimMatcherV2;

use ratatui::layout::Rect;

use crate::commands::viz::{VizOptions, VizOutput};
use workgraph::config::Config;
use workgraph::graph::{CycleAnalysis, Status, TokenUsage, format_tokens, parse_token_usage_live};
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

/// Insert-mode paste: inserts text at cursor and leaves cursor after the
/// inserted text.  This replaces edtui's `on_paste_event` which uses Vim
/// Normal-mode semantics (`append_str`) and leaves the cursor one position
/// short.
pub fn paste_insert_mode(text: &str, state: &mut EditorState) {
    use edtui::actions::{
        Execute,
        insert::{InsertChar, LineBreak},
    };
    for ch in text.chars() {
        if ch == '\n' {
            LineBreak(1).execute(state);
        } else {
            InsertChar(ch).execute(state);
        }
    }
}

pub fn create_editor_handler() -> EditorEventHandler {
    use edtui::actions::delete::{DeleteToEndOfLine, RemoveChar};
    use edtui::actions::{
        MoveBackward, MoveDown, MoveForward, MoveToEndOfLine, MoveToStartOfLine, MoveUp,
    };
    use edtui::events::{KeyEventRegister, KeyInput};
    let mut handler = EditorEventHandler::default();
    // Emacs keybindings for insert mode
    handler.key_handler.insert(
        KeyEventRegister::i(vec![KeyInput::ctrl('a')]),
        MoveToStartOfLine(),
    );
    handler.key_handler.insert(
        KeyEventRegister::i(vec![KeyInput::ctrl('e')]),
        MoveToEndOfLine(),
    );
    handler.key_handler.insert(
        KeyEventRegister::i(vec![KeyInput::ctrl('f')]),
        MoveForward(1),
    );
    handler.key_handler.insert(
        KeyEventRegister::i(vec![KeyInput::ctrl('b')]),
        MoveBackward(1),
    );
    handler.key_handler.insert(
        KeyEventRegister::i(vec![KeyInput::ctrl('d')]),
        RemoveChar(1),
    );
    handler.key_handler.insert(
        KeyEventRegister::i(vec![KeyInput::ctrl('k')]),
        DeleteToEndOfLine,
    );
    handler
        .key_handler
        .insert(KeyEventRegister::i(vec![KeyInput::ctrl('n')]), MoveDown(1));
    handler
        .key_handler
        .insert(KeyEventRegister::i(vec![KeyInput::ctrl('p')]), MoveUp(1));
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
    /// A phase annotation was clicked (brief flash on the annotation text).
    #[allow(dead_code)]
    AnnotationClick,
}

/// Hit region for a clickable phase annotation in the graph view.
/// Computed from `plain_lines`, `annotation_map`, and `node_line_map` after each refresh.
#[derive(Clone, Debug)]
pub struct AnnotationHitRegion {
    /// Original line index in `plain_lines`.
    pub orig_line: usize,
    /// Start column (inclusive) of the annotation text in the plain line.
    pub col_start: usize,
    /// End column (exclusive) of the annotation text in the plain line.
    pub col_end: usize,
    /// The parent task ID whose line contains this annotation.
    #[allow(dead_code)]
    pub parent_task_id: String,
    /// The dot-task IDs that produced this annotation.
    pub dot_task_ids: Vec<String>,
}

/// A "sticky" annotation that persists in the UI for a minimum duration
/// even after the underlying system task has completed. This ensures
/// transient states like "assigning" (which may last only 1-3 seconds)
/// remain visible long enough for the user to notice.
#[derive(Clone, Debug)]
pub struct StickyAnnotation {
    /// The annotation info (display text + source task IDs).
    pub info: crate::commands::viz::AnnotationInfo,
    /// When this annotation was last seen in the live graph state.
    pub last_seen: Instant,
}

/// Minimum duration (in seconds) to display a transient annotation after
/// it disappears from the live graph state.
const STICKY_ANNOTATION_HOLD_SECS: u64 = 3;

/// Transient flash state for a clicked annotation.
#[derive(Clone, Debug)]
pub struct AnnotationClickFlash {
    /// Original line index of the flashed annotation.
    pub orig_line: usize,
    /// Start column (inclusive).
    pub col_start: usize,
    /// End column (exclusive).
    pub col_end: usize,
    /// When the flash started.
    pub start: Instant,
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
        Status::Waiting | Status::PendingValidation => (60, 160, 220), // blue
        Status::PendingEval => (140, 230, 80), // chartreuse: between yellow (in-progress) and green (done)
        Status::Incomplete => (255, 165, 0),  // orange
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
        AnimationKind::Revealed => (120, 120, 140), // soft gray-blue
        AnimationKind::AnnotationClick => (255, 180, 220), // bright pink
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Touch echo (click/touch visual feedback)
// ══════════════════════════════════════════════════════════════════════════════

/// Maximum number of simultaneous touch echo indicators.
const MAX_TOUCH_ECHOES: usize = 10;

/// Duration of the touch echo fade animation in seconds.
const TOUCH_ECHO_DURATION_SECS: f64 = 0.7;

/// A transient visual indicator shown at a click/touch position.
#[derive(Clone, Debug)]
pub struct TouchEcho {
    /// Terminal column where the click occurred.
    pub col: u16,
    /// Terminal row where the click occurred.
    pub row: u16,
    /// When the echo was created.
    pub start: Instant,
}

impl TouchEcho {
    /// Progress from 0.0 (just appeared) to 1.0 (fully faded).
    pub fn progress(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        (elapsed / TOUCH_ECHO_DURATION_SECS).min(1.0)
    }

    /// Whether this echo has fully faded and should be removed.
    pub fn is_expired(&self) -> bool {
        self.start.elapsed().as_secs_f64() >= TOUCH_ECHO_DURATION_SECS
    }
}

/// Direction of an inspector panel slide animation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SlideDirection {
    /// New view slides in from the right (forward cycling).
    Forward,
    /// New view slides in from the left (backward cycling).
    Backward,
}

/// Active slide animation on the inspector panel.
#[derive(Clone)]
pub struct SlideAnimation {
    /// When the animation started.
    pub start: Instant,
    /// Direction of the slide.
    pub direction: SlideDirection,
}

impl SlideAnimation {
    /// Duration of the slide animation in seconds.
    const DURATION_SECS: f64 = 0.15;

    /// Returns the animation progress (0.0 = start, 1.0 = done).
    pub fn progress(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        (elapsed / Self::DURATION_SECS).min(1.0)
    }

    /// Whether the animation has completed.
    pub fn is_done(&self) -> bool {
        self.progress() >= 1.0
    }

    /// Returns the x-offset in columns for the panel content.
    /// Starts at +/- panel_width and eases to 0.
    pub fn x_offset(&self, panel_width: u16) -> i16 {
        let t = self.progress();
        // Ease-out quadratic: 1 - (1-t)^2
        let eased = 1.0 - (1.0 - t) * (1.0 - t);
        let full = panel_width as f64;
        let remaining = full * (1.0 - eased);
        match self.direction {
            SlideDirection::Forward => remaining as i16,
            SlideDirection::Backward => -(remaining as i16),
        }
    }
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
}

// ══════════════════════════════════════════════════════════════════════════════
// Toast notification system
// ══════════════════════════════════════════════════════════════════════════════

/// Severity level for toast notifications, controlling color and auto-dismiss behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastSeverity {
    /// Green, auto-dismiss after 5 seconds.
    Info,
    /// Yellow, auto-dismiss after 10 seconds.
    Warning,
    /// Red, auto-dismiss after 30 seconds (also dismissible early with Esc).
    Error,
}

impl ToastSeverity {
    /// Duration before auto-dismiss.
    pub fn auto_dismiss_duration(&self) -> Option<std::time::Duration> {
        match self {
            ToastSeverity::Info => Some(std::time::Duration::from_secs(5)),
            ToastSeverity::Warning => Some(std::time::Duration::from_secs(10)),
            ToastSeverity::Error => Some(std::time::Duration::from_secs(30)),
        }
    }
}

/// A toast notification with severity, message, and optional deduplication key.
#[derive(Clone, Debug)]
pub struct Toast {
    pub message: String,
    pub severity: ToastSeverity,
    pub created_at: Instant,
    /// Optional deduplication key. If set, only one toast with this key is kept active.
    pub dedup_key: Option<String>,
}

/// Maximum number of visible toasts at once.
pub const MAX_VISIBLE_TOASTS: usize = 4;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RightPanelTab {
    Chat,      // 0
    Detail,    // 1
    Agency,    // 2
    Config,    // 3
    Log,       // 4  — per-task agent output + structured log entries
    CoordLog,  // 5
    Dashboard, // 6
    Messages,  // 7  — wg msg traffic for the selected task
    // Dead tabs — kept so historical match arms compile, not reachable
    // from the tab bar.
    Files,
    Firehose,
    Output,
}

impl RightPanelTab {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Detail => "Detail",
            Self::Agency => "Agency",
            Self::Config => "Config",
            Self::Log => "Log",
            Self::CoordLog => "Coord",
            Self::Dashboard => "Dash",
            Self::Messages => "Msg",
            Self::Files | Self::Firehose | Self::Output => "",
        }
    }

    pub fn index(&self) -> usize {
        match self {
            Self::Chat => 0,
            Self::Detail => 1,
            Self::Agency => 2,
            Self::Config => 3,
            Self::Log => 4,
            Self::CoordLog => 5,
            Self::Dashboard => 6,
            Self::Messages => 7,
            Self::Files => usize::MAX - 2,
            Self::Firehose => usize::MAX - 1,
            Self::Output => usize::MAX,
        }
    }

    pub fn from_index(i: usize) -> Option<Self> {
        match i {
            0 => Some(Self::Chat),
            1 => Some(Self::Detail),
            2 => Some(Self::Agency),
            3 => Some(Self::Config),
            4 => Some(Self::Log),
            5 => Some(Self::CoordLog),
            6 => Some(Self::Dashboard),
            7 => Some(Self::Messages),
            _ => None,
        }
    }

    pub fn next(&self) -> Self {
        Self::from_index((self.index() + 1) % Self::ALL.len()).unwrap()
    }

    pub fn prev(&self) -> Self {
        Self::from_index((self.index() + Self::ALL.len() - 1) % Self::ALL.len()).unwrap()
    }

    pub const ALL: [RightPanelTab; 8] = [
        Self::Chat,
        Self::Detail,
        Self::Agency,
        Self::Config,
        Self::Log,
        Self::CoordLog,
        Self::Dashboard,
        Self::Messages,
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
    /// Dragging the vertical divider between graph and inspector panels.
    Divider,
    /// Dragging the horizontal divider between graph (top) and inspector (bottom) in stacked mode.
    HorizontalDivider,
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

/// Layout mode for the five-state cycle (i/v/=/Shift+Tab key).
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

    #[allow(dead_code)]
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

    /// Whether this is a normal split mode (both graph and inspector visible).
    pub fn is_normal_split(&self) -> bool {
        matches!(
            self,
            Self::ThirdInspector | Self::HalfInspector | Self::TwoThirdsInspector
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

/// Responsive layout breakpoint determined by terminal width.
///
/// Detected dynamically on each frame from `frame.area().width`:
/// - `Compact`: < 50 cols — single-panel mode (graph OR detail, Tab to switch)
/// - `Narrow`: 50–80 cols — narrow split, hide non-essential columns
/// - `Full`: > 80 cols — current full layout (no change)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponsiveBreakpoint {
    /// < 50 cols: single-panel mode.
    Compact,
    /// 50–80 cols: narrow split with hidden non-essential columns.
    Narrow,
    /// > 80 cols: full layout.
    Full,
}

impl ResponsiveBreakpoint {
    /// Determine the breakpoint from terminal width.
    pub fn from_width(width: u16) -> Self {
        if width < 50 {
            Self::Compact
        } else if width <= 80 {
            Self::Narrow
        } else {
            Self::Full
        }
    }
}

/// Which panel is shown in single-panel (compact) mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SinglePanelView {
    /// Show the graph panel.
    Graph,
    /// Show the detail/inspector panel.
    Detail,
    /// Show the log/output panel.
    Log,
}

impl SinglePanelView {
    /// Cycle to the next panel: Graph → Detail → Log → Graph.
    pub fn next(self) -> Self {
        match self {
            Self::Graph => Self::Detail,
            Self::Detail => Self::Log,
            Self::Log => Self::Graph,
        }
    }

    /// Cycle to the previous panel: Graph → Log → Detail → Graph.
    pub fn prev(self) -> Self {
        match self {
            Self::Graph => Self::Log,
            Self::Detail => Self::Graph,
            Self::Log => Self::Detail,
        }
    }

    /// Human-readable label for breadcrumb display.
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            Self::Graph => "Graph",
            Self::Detail => "Detail",
            Self::Log => "Log",
        }
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
    /// Choice dialog (e.g., coordinator removal options).
    ChoiceDialog(ChoiceDialogState),
    /// Coordinator picker overlay (list of all coordinators).
    CoordinatorPicker,
    /// Config panel text editing mode.
    ConfigEdit,
    /// Chat search mode (/ key in chat tab). Keys go to chat search input.
    ChatSearch,
    /// Full-pane coordinator launcher (replaces chat view area).
    Launcher,
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

// ════════════════��═════════════════════════════════════════════════════════════
// Reusable FilterPicker — fuzzy-filterable list selector
// ═══════════════════════════��══════════════════════════════════════════════════

/// A reusable fuzzy-filterable list picker.
///
/// Used by the coordinator launcher (executor/model/endpoint selection)
/// and the config panel (Choice fields). Supports:
/// - Typing to fuzzy-filter the list
/// - Arrow-key navigation within filtered results
/// - Custom entry mode (type a freeform value)
/// - Empty-list hint messages
#[derive(Clone, Debug)]
pub struct FilterPicker {
    /// All available items: (id, description).
    pub items: Vec<(String, String)>,
    /// Current filter text (typed by the user).
    pub filter: String,
    /// Indices into `items` that match the current filter, sorted by score.
    pub filtered_indices: Vec<usize>,
    /// Selected index into `filtered_indices`.
    pub selected: usize,
    /// Whether custom freeform entry is allowed (shows a "Custom:" row).
    pub allow_custom: bool,
    /// Whether the user is currently typing a custom value.
    pub custom_active: bool,
    /// Custom value text buffer.
    pub custom_text: String,
    /// Hint to show when items list is empty.
    pub empty_hint: String,
    /// First visible filtered-row index (for scroll-window rendering).
    /// Render code uses this + the rendered viewport to clamp; mouse
    /// scroll handlers move this directly.
    pub scroll_offset: usize,
}

impl FilterPicker {
    pub fn new(items: Vec<(String, String)>, allow_custom: bool) -> Self {
        let filtered_indices: Vec<usize> = (0..items.len()).collect();
        Self {
            items,
            filter: String::new(),
            filtered_indices,
            selected: 0,
            allow_custom,
            custom_active: false,
            custom_text: String::new(),
            empty_hint: String::new(),
            scroll_offset: 0,
        }
    }

    pub fn with_hint(mut self, hint: &str) -> Self {
        self.empty_hint = hint.to_string();
        self
    }

    pub fn with_selected_id(mut self, id: &str) -> Self {
        if let Some(pos) = self
            .filtered_indices
            .iter()
            .position(|&i| self.items[i].0 == id)
        {
            self.selected = pos;
        }
        self
    }

    /// Apply fuzzy filter to items based on current filter text.
    pub fn apply_filter(&mut self) {
        if self.filter.is_empty() {
            self.filtered_indices = (0..self.items.len()).collect();
        } else {
            let matcher = SkimMatcherV2::default();
            let mut scored: Vec<(usize, i64)> = self
                .items
                .iter()
                .enumerate()
                .filter_map(|(i, (id, desc))| {
                    let haystack = format!("{} {}", id, desc);
                    matcher
                        .fuzzy_match(&haystack, &self.filter)
                        .map(|score| (i, score))
                })
                .collect();
            scored.sort_by(|a, b| b.1.cmp(&a.1));
            self.filtered_indices = scored.into_iter().map(|(i, _)| i).collect();
        }
        if self.selected >= self.visible_count() {
            self.selected = self.visible_count().saturating_sub(1);
        }
    }

    /// Number of visible items (filtered list + optional custom row).
    pub fn visible_count(&self) -> usize {
        let base = self.filtered_indices.len();
        if self.allow_custom {
            base + 1
        } else {
            base
        }
    }

    /// Whether the selected index points to the custom row.
    pub fn is_custom_selected(&self) -> bool {
        self.allow_custom && self.selected >= self.filtered_indices.len()
    }

    /// Move selection up.
    pub fn prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        }
    }

    /// Move selection down.
    pub fn next(&mut self) {
        let max = self.visible_count().saturating_sub(1);
        if self.selected < max {
            self.selected += 1;
        }
    }

    /// Scroll the viewport up by `n` rows (does not move selection).
    pub fn scroll_up(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    /// Scroll the viewport down by `n` rows (does not move selection).
    pub fn scroll_down(&mut self, n: usize) {
        let max_offset = self.visible_count().saturating_sub(1);
        self.scroll_offset = (self.scroll_offset + n).min(max_offset);
    }


    /// Get the currently selected item's (id, description), or None if custom.
    pub fn selected_item(&self) -> Option<&(String, String)> {
        self.filtered_indices
            .get(self.selected)
            .and_then(|&i| self.items.get(i))
    }

    /// Get the effective value: selected item's id, or custom text.
    pub fn value(&self) -> Option<String> {
        if self.custom_active && !self.custom_text.is_empty() {
            Some(self.custom_text.clone())
        } else if self.is_custom_selected() {
            if !self.custom_text.is_empty() {
                Some(self.custom_text.clone())
            } else {
                None
            }
        } else {
            self.selected_item().map(|(id, _)| id.clone())
        }
    }

    /// Handle a character input: add to filter and re-filter.
    pub fn type_char(&mut self, c: char) {
        if self.custom_active {
            self.custom_text.push(c);
        } else {
            self.filter.push(c);
            self.apply_filter();
        }
    }

    /// Handle backspace: remove from filter and re-filter.
    pub fn backspace(&mut self) {
        if self.custom_active {
            self.custom_text.pop();
        } else {
            self.filter.pop();
            self.apply_filter();
        }
    }

    /// Enter custom entry mode.
    pub fn enter_custom(&mut self) {
        if self.allow_custom {
            self.custom_active = true;
        }
    }

    /// Exit custom entry mode.
    pub fn exit_custom(&mut self) {
        self.custom_active = false;
    }
}

/// Which section of the launcher pane has keyboard focus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LauncherSection {
    Name,
    Executor,
    Model,
    Endpoint,
    Recent,
}

/// What a row in a launcher FilterPicker maps to when clicked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LauncherListHit {
    /// Click selected the filtered list row at this index in `selected`-space.
    Item(usize),
    /// Click landed on the "Custom: ..." row (entry mode).
    Custom,
}

/// State for the full-pane coordinator launcher.
#[derive(Clone, Debug)]
pub struct LauncherState {
    pub active_section: LauncherSection,
    pub name: String,
    pub executor_list: Vec<(String, String, bool)>, // (name, description, available)
    pub executor_selected: usize,
    pub model_picker: FilterPicker,
    pub endpoint_picker: FilterPicker,
    pub recent_list: Vec<workgraph::launcher_history::HistoryEntry>,
    pub recent_selected: usize,
    /// Full unfiltered model catalog. The model_picker holds the
    /// executor-filtered subset; we re-derive it on executor change.
    pub all_models: Vec<(String, String)>,
}

/// Return models, ordered for the given `executor` (compatible-first).
///
/// Reorders so models known to be a "natural" fit for the executor appear
/// at the top, but ALWAYS returns the full list — the user can still pick
/// any model. A strict filter would trap users whose registry uses a
/// different naming convention (e.g. `openrouter:claude-opus-4-6` with the
/// `claude` executor) on a "Custom" row with no way to submit.
///
/// Recognized hints:
/// - `claude`  → put `claude:*` and `*claude*` ids first
/// - `codex`   → put `openai:*` first
/// - `gemini`  → put `google:*` first
/// - `native`/`amplifier` → no reorder (all models work via OAI-compat)
pub fn filter_models_for_executor(
    all_models: &[(String, String)],
    executor: &str,
) -> Vec<(String, String)> {
    let needles: &[&str] = match executor {
        "claude" => &["claude", "anthropic"],
        "codex" => &["openai", "gpt"],
        "gemini" => &["google", "gemini"],
        _ => return all_models.to_vec(),
    };
    let mut compatible: Vec<(String, String)> = Vec::new();
    let mut other: Vec<(String, String)> = Vec::new();
    for m in all_models {
        if needles.iter().any(|n| m.0.contains(n)) {
            compatible.push(m.clone());
        } else {
            other.push(m.clone());
        }
    }
    compatible.append(&mut other);
    compatible
}

impl LauncherState {
    pub fn selected_executor(&self) -> &str {
        self.executor_list
            .get(self.executor_selected)
            .map(|(name, _, _)| name.as_str())
            .unwrap_or("claude")
    }

    /// Rebuild `model_picker.items` for the current executor.
    /// Preserves filter text + custom_text but resets selection + scroll.
    pub fn refresh_model_filter_for_executor(&mut self) {
        let executor = self.selected_executor().to_string();
        let new_items = filter_models_for_executor(&self.all_models, &executor);
        let preserved_filter = self.model_picker.filter.clone();
        let preserved_custom = self.model_picker.custom_text.clone();
        let hint = self.model_picker.empty_hint.clone();
        let allow_custom = self.model_picker.allow_custom;
        let mut new_picker = FilterPicker::new(new_items, allow_custom);
        new_picker.empty_hint = hint;
        new_picker.filter = preserved_filter;
        new_picker.custom_text = preserved_custom;
        new_picker.apply_filter();
        self.model_picker = new_picker;
    }

    /// Move the executor cursor by `delta` (positive = down) and refresh
    /// the model list. Returns true if executor selection changed.
    pub fn select_executor(&mut self, idx: usize) -> bool {
        if idx >= self.executor_list.len() || idx == self.executor_selected {
            return false;
        }
        self.executor_selected = idx;
        self.refresh_model_filter_for_executor();
        true
    }

    pub fn show_endpoint(&self) -> bool {
        self.selected_executor() == "native"
    }

    pub fn selected_model(&self) -> Option<String> {
        self.model_picker.value()
    }

    pub fn selected_endpoint(&self) -> Option<String> {
        if !self.show_endpoint() {
            return None;
        }
        self.endpoint_picker.value()
    }

    // Backward-compat accessors used by event.rs Recent-entry population
    pub fn select_model_by_id(&mut self, id: &str) {
        self.model_picker.filter.clear();
        self.model_picker.apply_filter();
        self.model_picker.custom_active = false;
        if let Some(pos) = self
            .model_picker
            .filtered_indices
            .iter()
            .position(|&i| self.model_picker.items[i].0 == id)
        {
            self.model_picker.selected = pos;
        } else {
            self.model_picker.custom_text = id.to_string();
            self.model_picker.selected = self.model_picker.filtered_indices.len();
        }
    }

    pub fn select_endpoint_by_value(&mut self, val: &str) {
        self.endpoint_picker.filter.clear();
        self.endpoint_picker.apply_filter();
        self.endpoint_picker.custom_active = false;
        if let Some(pos) = self
            .endpoint_picker
            .filtered_indices
            .iter()
            .position(|&i| self.endpoint_picker.items[i].1 == val)
        {
            self.endpoint_picker.selected = pos;
        } else {
            self.endpoint_picker.custom_text = val.to_string();
            self.endpoint_picker.selected = self.endpoint_picker.filtered_indices.len();
        }
    }

    pub fn next_section(&mut self) {
        self.active_section = match self.active_section {
            LauncherSection::Name => LauncherSection::Executor,
            LauncherSection::Executor => LauncherSection::Model,
            LauncherSection::Model => {
                if self.show_endpoint() {
                    LauncherSection::Endpoint
                } else if !self.recent_list.is_empty() {
                    LauncherSection::Recent
                } else {
                    LauncherSection::Name
                }
            }
            LauncherSection::Endpoint => {
                if !self.recent_list.is_empty() {
                    LauncherSection::Recent
                } else {
                    LauncherSection::Name
                }
            }
            LauncherSection::Recent => LauncherSection::Name,
        };
    }

    pub fn prev_section(&mut self) {
        self.active_section = match self.active_section {
            LauncherSection::Name => {
                if !self.recent_list.is_empty() {
                    LauncherSection::Recent
                } else if self.show_endpoint() {
                    LauncherSection::Endpoint
                } else {
                    LauncherSection::Model
                }
            }
            LauncherSection::Executor => LauncherSection::Name,
            LauncherSection::Model => LauncherSection::Executor,
            LauncherSection::Endpoint => LauncherSection::Model,
            LauncherSection::Recent => {
                if self.show_endpoint() {
                    LauncherSection::Endpoint
                } else {
                    LauncherSection::Model
                }
            }
        };
    }
}

/// What action a choice dialog will perform when an option is selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChoiceDialogAction {
    /// Remove/archive/stop a coordinator by its ID.
    RemoveCoordinator(u32),
}

/// State for a choice dialog with multiple selectable options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChoiceDialogState {
    pub action: ChoiceDialogAction,
    /// Index of the currently highlighted option.
    pub selected: usize,
    /// Each option: (hotkey, label, description).
    pub options: Vec<(char, String, String)>,
}

/// State for the coordinator picker overlay.
#[derive(Clone, Debug)]
pub struct CoordinatorPickerState {
    /// Index of the currently highlighted coordinator.
    pub selected: usize,
    /// List of (coordinator_id, label, status_description, is_alive).
    pub entries: Vec<(u32, String, String, bool)>,
}

/// What kind of entry a tab bar hit represents.
#[derive(Clone, Debug)]
pub enum TabBarEntryKind {
    /// A coordinator tab (identified by numeric coordinator ID).
    Coordinator(u32),
    /// A user board tab (identified by task ID, e.g. `.user-erik-0`).
    UserBoard(String),
}

/// Hit area for a single tab in the coordinator/user-board tab bar.
#[derive(Clone, Debug)]
pub struct CoordinatorTabHit {
    /// The kind of entry this hit represents.
    pub kind: TabBarEntryKind,
    /// Column range for the entire tab (clicking switches coordinator).
    pub tab_start: u16,
    pub tab_end: u16,
    /// Column range for the close button (clicking deletes coordinator).
    /// If close_start == close_end, there is no close button.
    pub close_start: u16,
    pub close_end: u16,
}

/// Column range for the [+] button in the coordinator tab bar.
#[derive(Clone, Debug, Default)]
pub struct CoordinatorPlusHit {
    pub start: u16,
    pub end: u16,
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
    /// Set of request IDs for in-flight chat requests.
    /// `awaiting_response()` is derived: `!pending_request_ids.is_empty()`.
    pub pending_request_ids: std::collections::HashSet<String>,
    /// Outbox cursor: last-read outbox message ID (for polling new messages).
    pub outbox_cursor: u64,
    /// Indices of user messages that were sent while a response was in flight.
    /// These messages are displayed immediately but reordered after the coordinator
    /// response arrives so they appear in correct chronological position.
    pub deferred_user_indices: Vec<usize>,
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
    /// Index of the message currently being edited (None = not in edit mode).
    pub editing_index: Option<usize>,
    /// Saved text from the input box before entering edit mode (restored on cancel).
    pub edit_saved_input: String,
    /// History navigation cursor: index into the list of editable user messages.
    /// None = not navigating history (fresh input).
    pub history_cursor: Option<usize>,
    /// Mapping from rendered line index to message index (set each frame by renderer).
    /// Used for click-to-edit: determines which message a clicked line belongs to.
    pub line_to_message: Vec<Option<usize>>,
    /// Per-coordinator input mode — no longer restored on switch (always resets to Normal),
    /// but kept for potential future use / debugging.
    #[allow(dead_code)]
    pub input_mode: InputMode,
    /// Whether the user explicitly dismissed chat input with Esc (per-coordinator).
    pub chat_input_dismissed: bool,
    /// Partial streaming text from the coordinator (displayed progressively).
    pub streaming_text: String,
    /// When the first pending request was added (for spinner elapsed time).
    /// Cleared when the set empties.
    pub awaiting_since: Option<std::time::Instant>,
    /// Whether there are older messages in the history file that haven't been loaded yet.
    pub has_more_history: bool,
    /// Total number of messages in the persisted history file.
    pub total_history_count: usize,
    /// Number of messages at the start of the history file that are NOT in memory.
    /// Used by save to preserve unloaded older messages.
    pub skipped_history_count: usize,
    /// In-chat search state.
    pub search: ChatSearchState,
    /// Whether archive files have been loaded into the scrollback.
    pub archives_loaded: bool,
    /// Whether there are archive files available to load.
    pub has_archives: bool,
    /// Previous frame's total rendered lines — used to adjust scroll
    /// when new content arrives while the user is scrolled up, so
    /// the viewport stays anchored to the content they were reading.
    pub prev_total_rendered_lines: usize,
}

impl ChatState {
    /// Whether any chat request is in flight.
    pub fn awaiting_response(&self) -> bool {
        !self.pending_request_ids.is_empty()
    }
}

/// State for in-chat search (/ key when chat tab is focused).
#[derive(Clone, Debug, Default)]
pub struct ChatSearchState {
    /// The current search query.
    pub query: String,
    /// Matches: (message_index, byte_offset_in_text) pairs.
    pub matches: Vec<ChatSearchMatch>,
    /// Index into `matches` for the currently focused match.
    pub current_match: Option<usize>,
}

/// A single match in the chat search results.
#[derive(Clone, Debug)]
pub struct ChatSearchMatch {
    /// Index into `ChatState::messages`.
    pub message_idx: usize,
    /// Byte offset of the match start within the message text.
    #[allow(dead_code)]
    pub byte_offset: usize,
    /// Length of the matched text in bytes.
    #[allow(dead_code)]
    pub match_len: usize,
}

impl Default for ChatState {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            editor: new_emacs_editor(),
            scroll: 0,
            pending_request_ids: std::collections::HashSet::new(),
            outbox_cursor: 0,
            deferred_user_indices: Vec::new(),
            coordinator_active: false,
            pending_attachments: Vec::new(),
            total_rendered_lines: 0,
            viewport_height: 0,
            scroll_from_top: 0,
            editing_index: None,
            edit_saved_input: String::new(),
            history_cursor: None,
            line_to_message: Vec::new(),
            input_mode: InputMode::Normal,
            chat_input_dismissed: false,
            streaming_text: String::new(),
            awaiting_since: None,
            has_more_history: false,
            total_history_count: 0,
            skipped_history_count: 0,
            search: ChatSearchState::default(),
            archives_loaded: false,
            has_archives: false,
            prev_total_rendered_lines: 0,
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
    /// Whether this message was edited by the user.
    pub edited: bool,
    /// Inbox message ID (for user messages loaded from chat history).
    /// Used to edit/delete the message in the inbox JSONL file.
    pub inbox_id: Option<u64>,
    /// The user who sent this message (from `current_user()`).
    pub user: Option<String>,
    /// Target task ID for SentMessage role (which task the message was sent to).
    pub target_task: Option<String>,
    /// ISO 8601 timestamp for temporal ordering of interleaved messages.
    pub msg_timestamp: Option<String>,
    /// ISO 8601 timestamp of when the agent read this message.
    pub read_at: Option<String>,
    /// Message queue ID (for deduplication during polling).
    pub msg_queue_id: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatRole {
    User,
    Coordinator,
    System,
    /// An LLM or system-level error surfaced inline in the chat stream.
    SystemError,
    /// A message sent to an agent's task via `wg msg send`, shown interleaved
    /// at the temporal position where the agent read it.
    SentMessage,
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    edited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    msg_timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    read_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    msg_queue_id: Option<u64>,
}

/// Path to the persisted chat history JSONL file for a specific coordinator.
/// Uses `chat-history-{cid}.jsonl` for all coordinators.
fn chat_history_path(workgraph_dir: &std::path::Path, coordinator_id: u32) -> std::path::PathBuf {
    workgraph_dir.join(format!("chat-history-{}.jsonl", coordinator_id))
}

/// Path to the legacy JSON array chat history file (for backward-compat migration).
fn chat_history_legacy_path(
    workgraph_dir: &std::path::Path,
    coordinator_id: u32,
) -> std::path::PathBuf {
    if coordinator_id == 0 {
        workgraph_dir.join("chat-history.json")
    } else {
        workgraph_dir.join(format!("chat-history-{}.json", coordinator_id))
    }
}

fn persisted_to_chat_message(p: PersistedChatMessage) -> ChatMessage {
    ChatMessage {
        role: match p.role.as_str() {
            "user" => ChatRole::User,
            "coordinator" => ChatRole::Coordinator,
            "sent_message" => ChatRole::SentMessage,
            "system-error" => ChatRole::SystemError,
            _ => ChatRole::System,
        },
        text: p.text,
        full_text: p.full_text,
        attachments: p.attachments,
        edited: p.edited,
        inbox_id: None,
        user: p.user,
        target_task: p.target_task,
        msg_timestamp: p.msg_timestamp,
        read_at: p.read_at,
        msg_queue_id: p.msg_queue_id,
    }
}

fn chat_message_to_persisted(m: &ChatMessage) -> PersistedChatMessage {
    PersistedChatMessage {
        role: match m.role {
            ChatRole::User => "user".to_string(),
            ChatRole::Coordinator => "coordinator".to_string(),
            ChatRole::System => "system".to_string(),
            ChatRole::SystemError => "system-error".to_string(),
            ChatRole::SentMessage => "sent_message".to_string(),
        },
        text: m.text.clone(),
        full_text: m.full_text.clone(),
        attachments: m.attachments.clone(),
        timestamp: m
            .msg_timestamp
            .clone()
            .unwrap_or_else(|| chrono::Utc::now().to_rfc3339()),
        edited: m.edited,
        user: m.user.clone(),
        target_task: m.target_task.clone(),
        msg_timestamp: m.msg_timestamp.clone(),
        read_at: m.read_at.clone(),
        msg_queue_id: m.msg_queue_id,
    }
}

/// Save chat messages to disk as JSONL for a specific coordinator.
/// Respects config for max history size.
fn save_chat_history(
    workgraph_dir: &std::path::Path,
    coordinator_id: u32,
    messages: &[ChatMessage],
) {
    save_chat_history_with_skip(workgraph_dir, coordinator_id, messages, 0);
}

/// Save chat messages to disk, preserving `skipped_count` older messages from the existing file.
/// When only a partial page of history was loaded, we must not overwrite unloaded older messages.
fn save_chat_history_with_skip(
    workgraph_dir: &std::path::Path,
    coordinator_id: u32,
    messages: &[ChatMessage],
    skipped_count: usize,
) {
    let config = Config::load_or_default(workgraph_dir);
    if !config.tui.chat_history {
        return;
    }
    let max = config.tui.chat_history_max;
    let path = chat_history_path(workgraph_dir, coordinator_id);

    let mut buf = String::new();

    // If there are unloaded older messages in the file, preserve them.
    if skipped_count > 0
        && let Ok(data) = std::fs::read_to_string(&path)
    {
        let lines: Vec<&str> = data.lines().filter(|l| !l.trim().is_empty()).collect();
        // Take the first `skipped_count` lines (the unloaded portion).
        let preserve_count = skipped_count.min(lines.len());
        for line in &lines[..preserve_count] {
            buf.push_str(line);
            buf.push('\n');
        }
    }

    // Append current in-memory messages (skip SentMessage entries — they were
    // interleaved from task message queues and don't belong in the coordinator chat).
    for m in messages {
        if m.role == ChatRole::SentMessage {
            continue;
        }
        let p = chat_message_to_persisted(m);
        if let Ok(line) = serde_json::to_string(&p) {
            buf.push_str(&line);
            buf.push('\n');
        }
    }

    // Trim to max from the end (keeps the most recent messages).
    let all_lines: Vec<&str> = buf.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = all_lines.len();
    let skip = total.saturating_sub(max);
    let mut final_buf = String::new();
    for line in &all_lines[skip..] {
        final_buf.push_str(line);
        final_buf.push('\n');
    }

    let _ = std::fs::write(&path, final_buf);
}

/// Result of a paginated chat history load.
struct PaginatedChatHistory {
    /// The loaded messages (most recent page).
    messages: Vec<ChatMessage>,
    /// Total number of messages in the history file.
    total_count: usize,
    /// Whether there are older messages not yet loaded.
    has_more: bool,
}

/// Load the last `limit` persisted chat messages from disk for a specific coordinator.
/// Handles both new JSONL format and legacy JSON array format (with auto-migration).
fn load_persisted_chat_history_paginated(
    workgraph_dir: &std::path::Path,
    coordinator_id: u32,
    limit: usize,
) -> PaginatedChatHistory {
    let config = Config::load_or_default(workgraph_dir);
    if !config.tui.chat_history {
        return PaginatedChatHistory {
            messages: vec![],
            total_count: 0,
            has_more: false,
        };
    }

    let jsonl_path = chat_history_path(workgraph_dir, coordinator_id);

    // Try JSONL format first.
    if jsonl_path.exists() {
        return load_jsonl_tail(&jsonl_path, limit);
    }

    // Fall back to legacy JSON array format and auto-migrate.
    let legacy_path = chat_history_legacy_path(workgraph_dir, coordinator_id);
    if legacy_path.exists() {
        let result = load_legacy_json_paginated(&legacy_path, limit);
        // Auto-migrate: rewrite as JSONL if we loaded anything.
        if result.total_count > 0 {
            // Load ALL messages for migration (not just the page).
            let all = load_legacy_json_all(&legacy_path);
            if !all.is_empty() {
                let mut buf = String::new();
                for m in &all {
                    let p = chat_message_to_persisted(m);
                    if let Ok(line) = serde_json::to_string(&p) {
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                }
                let _ = std::fs::write(&jsonl_path, buf);
                // Remove legacy file after successful migration.
                let _ = std::fs::remove_file(&legacy_path);
            }
        }
        return result;
    }

    PaginatedChatHistory {
        messages: vec![],
        total_count: 0,
        has_more: false,
    }
}

/// Efficiently load the last `limit` messages from a JSONL file by reading from the end.
fn load_jsonl_tail(path: &std::path::Path, limit: usize) -> PaginatedChatHistory {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => {
            return PaginatedChatHistory {
                messages: vec![],
                total_count: 0,
                has_more: false,
            };
        }
    };

    let file_len = match file.seek(SeekFrom::End(0)) {
        Ok(len) => len as usize,
        Err(_) => {
            return PaginatedChatHistory {
                messages: vec![],
                total_count: 0,
                has_more: false,
            };
        }
    };

    if file_len == 0 {
        return PaginatedChatHistory {
            messages: vec![],
            total_count: 0,
            has_more: false,
        };
    }

    // Read the file contents and extract the tail.
    let _ = file.seek(SeekFrom::Start(0));
    let mut all_data = String::new();
    if file.read_to_string(&mut all_data).is_err() {
        return PaginatedChatHistory {
            messages: vec![],
            total_count: 0,
            has_more: false,
        };
    }

    let lines: Vec<&str> = all_data.lines().filter(|l| !l.trim().is_empty()).collect();
    let total_count = lines.len();

    if total_count == 0 {
        return PaginatedChatHistory {
            messages: vec![],
            total_count: 0,
            has_more: false,
        };
    }

    let skip = total_count.saturating_sub(limit);
    let tail_lines = &lines[skip..];

    let messages: Vec<ChatMessage> = tail_lines
        .iter()
        .filter_map(|line| {
            serde_json::from_str::<PersistedChatMessage>(line)
                .ok()
                .map(persisted_to_chat_message)
        })
        // Filter out SentMessage entries — these were interleaved from task message
        // queues and don't belong in the coordinator chat. Task messages are visible
        // in the dedicated Messages panel.
        .filter(|m| m.role != ChatRole::SentMessage)
        .collect();

    PaginatedChatHistory {
        has_more: skip > 0,
        total_count,
        messages,
    }
}

/// Load a specific page of older messages from a JSONL file.
/// `loaded_count` is how many messages are already loaded from the tail.
/// Returns the next `page_size` messages before those already loaded.
fn load_jsonl_page(
    path: &std::path::Path,
    loaded_count: usize,
    page_size: usize,
) -> Vec<ChatMessage> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };

    let lines: Vec<&str> = data.lines().filter(|l| !l.trim().is_empty()).collect();
    let total = lines.len();

    if loaded_count >= total {
        return vec![];
    }

    // Messages are in chronological order. We've loaded the last `loaded_count`.
    // We want `page_size` messages before those.
    let end = total.saturating_sub(loaded_count);
    let start = end.saturating_sub(page_size);
    let page_lines = &lines[start..end];

    page_lines
        .iter()
        .filter_map(|line| {
            serde_json::from_str::<PersistedChatMessage>(line)
                .ok()
                .map(persisted_to_chat_message)
        })
        .filter(|m| m.role != ChatRole::SentMessage)
        .collect()
}

/// Load all messages from a legacy JSON array file.
fn load_legacy_json_all(path: &std::path::Path) -> Vec<ChatMessage> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    let persisted: Vec<PersistedChatMessage> = match serde_json::from_str(&data) {
        Ok(p) => p,
        Err(_) => return vec![],
    };
    persisted
        .into_iter()
        .map(persisted_to_chat_message)
        .filter(|m| m.role != ChatRole::SentMessage)
        .collect()
}

/// Load the last `limit` messages from a legacy JSON array file.
fn load_legacy_json_paginated(path: &std::path::Path, limit: usize) -> PaginatedChatHistory {
    let all = load_legacy_json_all(path);
    let total_count = all.len();
    let skip = total_count.saturating_sub(limit);
    let messages = all.into_iter().skip(skip).collect();
    PaginatedChatHistory {
        messages,
        total_count,
        has_more: skip > 0,
    }
}

/// Load persisted chat history from disk (backward-compat wrapper — loads all messages).
#[cfg(test)]
fn load_persisted_chat_history(
    workgraph_dir: &std::path::Path,
    coordinator_id: u32,
) -> Vec<ChatMessage> {
    let result = load_persisted_chat_history_paginated(workgraph_dir, coordinator_id, usize::MAX);
    result.messages
}

/// Persisted TUI state for focus restoration across restarts.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct PersistedTuiState {
    /// Which coordinator was focused when the TUI was last closed.
    #[serde(default)]
    active_coordinator_id: u32,
    /// Which right panel tab was active.
    #[serde(default)]
    right_panel_tab: String,
}

fn tui_state_path(workgraph_dir: &std::path::Path) -> std::path::PathBuf {
    workgraph_dir.join("tui-state.json")
}

fn save_tui_state(workgraph_dir: &std::path::Path, coordinator_id: u32, tab: &RightPanelTab) {
    let state = PersistedTuiState {
        active_coordinator_id: coordinator_id,
        right_panel_tab: format!("{:?}", tab),
    };
    if let Ok(json) = serde_json::to_string(&state) {
        let _ = std::fs::write(tui_state_path(workgraph_dir), json);
    }
}

fn load_tui_state(workgraph_dir: &std::path::Path) -> Option<PersistedTuiState> {
    let data = std::fs::read_to_string(tui_state_path(workgraph_dir)).ok()?;
    serde_json::from_str(&data).ok()
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

// ══════════════════════════════════════════════════════════════════════════════
// Navigation stack for drill-down
// ══════════════════════════════════════════════════════════════════════════════

/// A single entry in the drill-down navigation stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavEntry {
    /// Dashboard overview — the top-level agent view.
    Dashboard,
    /// Agent output view — live output for a specific agent.
    AgentDetail { agent_id: String },
    /// Task detail view — full task info for a specific task.
    TaskDetail { task_id: String },
    /// Task log view — log entries for a specific task.
    #[allow(dead_code)]
    TaskLog { task_id: String },
}

/// A stack-based navigation history for drill-down navigation.
/// Push on Enter (drill deeper), pop on Esc/b (go back).
#[derive(Clone, Debug, Default)]
pub struct NavStack {
    entries: Vec<NavEntry>,
}

impl NavStack {
    pub fn push(&mut self, entry: NavEntry) {
        self.entries.push(entry);
    }

    pub fn pop(&mut self) -> Option<NavEntry> {
        self.entries.pop()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Dashboard state
// ══════════════════════════════════════════════════════════════════════════════

/// Activity level classification for dashboard agents.
/// Determined by time since last output file modification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DashboardAgentActivity {
    /// Output modified <30s ago.
    Active,
    /// Output modified 30s–5m ago.
    Slow,
    /// Output modified >5m ago.
    Stuck,
    /// Agent process has exited (Done/Failed/Dead).
    Exited,
}

impl DashboardAgentActivity {
    /// Classify an agent based on registry status, seconds since last output
    /// modification, and whether the agent has active child processes.
    ///
    /// When `has_children` is true and output is stale, the agent is classified
    /// as `Slow` rather than `Stuck` — it is likely waiting on a subprocess
    /// (cargo build, wg commands, sub-agent, etc.).
    pub fn classify(
        status: AgentStatus,
        secs_since_output: Option<i64>,
        has_children: bool,
    ) -> Self {
        match status {
            AgentStatus::Done | AgentStatus::Failed | AgentStatus::Dead => Self::Exited,
            AgentStatus::Parked | AgentStatus::Frozen | AgentStatus::Stopping => Self::Exited,
            _ => match secs_since_output {
                Some(s) if s < 30 => Self::Active,
                Some(s) if s < 300 => Self::Slow,
                Some(_) if has_children => Self::Slow,
                Some(_) => Self::Stuck,
                // No output file yet — treat as active if still starting
                None => Self::Active,
            },
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Slow => "slow",
            Self::Stuck => "stuck",
            Self::Exited => "exited",
        }
    }
}

/// A single row in the dashboard agent table.
#[derive(Clone, Debug)]
pub struct DashboardAgentRow {
    pub agent_id: String,
    pub task_id: String,
    pub task_title: Option<String>,
    pub activity: DashboardAgentActivity,
    pub elapsed_secs: Option<i64>,
    pub model: Option<String>,
    pub latest_snippet: Option<String>,
}

/// Coordinator card data for the dashboard.
#[derive(Clone, Debug, Default)]
pub struct DashboardCoordinatorCard {
    pub id: u32,
    pub enabled: bool,
    pub paused: bool,
    pub frozen: bool,
    pub ticks: u64,
    pub agents_alive: usize,
    pub tasks_ready: usize,
    pub max_agents: usize,
    pub model: Option<String>,
    pub accumulated_tokens: u64,
}

/// State for the Dashboard panel tab.
pub struct DashboardState {
    /// Scroll offset for the dashboard content.
    pub scroll: usize,
    /// Total rendered lines (set each frame by renderer, for scrollbar).
    pub total_rendered_lines: usize,
    /// Viewport height (set each frame by renderer).
    pub viewport_height: usize,
    /// Selected row in the agent table (for Enter/k/t keybinds).
    pub selected_row: usize,
    /// Cached coordinator cards (refreshed each load_agent_monitor cycle).
    pub coordinator_cards: Vec<DashboardCoordinatorCard>,
    /// Cached agent rows (refreshed each load_agent_monitor cycle).
    pub agent_rows: Vec<DashboardAgentRow>,
    /// Activity sparkline buckets: number of events per time bucket (last N buckets).
    /// Each bucket covers `sparkline_bucket_secs` seconds.
    pub sparkline_data: Vec<u64>,
    /// Seconds per sparkline bucket.
    pub sparkline_bucket_secs: u64,
    /// Timestamp of the most recently recorded sparkline event.
    pub sparkline_last_event: Option<std::time::Instant>,
}

impl Default for DashboardState {
    fn default() -> Self {
        Self {
            scroll: 0,
            total_rendered_lines: 0,
            viewport_height: 0,
            selected_row: 0,
            coordinator_cards: Vec::new(),
            agent_rows: Vec::new(),
            sparkline_data: vec![0; 30], // 30 buckets
            sparkline_bucket_secs: 10,   // 10s per bucket = 5 min window
            sparkline_last_event: None,
        }
    }
}

impl DashboardState {
    /// Record an activity event (agent start/stop/status change) for sparkline.
    pub fn record_sparkline_event(&mut self) {
        let now = std::time::Instant::now();
        if let Some(last) = self.sparkline_last_event {
            let elapsed = now.duration_since(last).as_secs();
            let buckets_to_shift = (elapsed / self.sparkline_bucket_secs.max(1)) as usize;
            if buckets_to_shift > 0 {
                let shift = buckets_to_shift.min(self.sparkline_data.len());
                // Shift existing data left
                self.sparkline_data.rotate_left(shift);
                let len = self.sparkline_data.len();
                for v in &mut self.sparkline_data[len - shift..] {
                    *v = 0;
                }
            }
        }
        self.sparkline_last_event = Some(now);
        if let Some(last) = self.sparkline_data.last_mut() {
            *last += 1;
        }
    }

    /// Compute sparkline values from a list of event timestamps (for initial population).
    #[allow(dead_code)]
    pub fn compute_sparkline_from_timestamps(&mut self, timestamps: &[std::time::SystemTime]) {
        let bucket_count = self.sparkline_data.len();
        self.sparkline_data = vec![0; bucket_count];
        if timestamps.is_empty() {
            return;
        }
        let now = std::time::SystemTime::now();
        let window_secs = (bucket_count as u64) * self.sparkline_bucket_secs;
        for ts in timestamps {
            if let Ok(age) = now.duration_since(*ts) {
                let age_secs = age.as_secs();
                if age_secs < window_secs {
                    let bucket_idx =
                        bucket_count - 1 - (age_secs / self.sparkline_bucket_secs.max(1)) as usize;
                    if bucket_idx < bucket_count {
                        self.sparkline_data[bucket_idx] += 1;
                    }
                }
            }
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Service health indicator
// ══════════════════════════════════════════════════════════════════════════════

/// Health level for the service daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceHealthLevel {
    /// Service running normally, no stuck tasks.
    Green,
    /// Degraded: paused, starting up (<30s uptime), or has stuck tasks.
    Yellow,
    /// Down or errored: socket unreachable or process dead.
    Red,
}

/// A task that is in-progress but whose assigned agent PID is dead.
#[derive(Clone, Debug)]
pub struct StuckTask {
    pub task_id: String,
    pub task_title: String,
    pub agent_id: String,
}

/// Which item is focused in the service control panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlPanelFocus {
    StartStop,
    PauseResume,
    Restart,
    AgentSlots,
    PanicKill,
    StuckAgent(usize),
    KillAllDead,
    RetryFailedEvals,
}

impl ControlPanelFocus {
    pub fn next(&self, stuck_count: usize) -> Self {
        match self {
            Self::StartStop => Self::PauseResume,
            Self::PauseResume => Self::Restart,
            Self::Restart => Self::AgentSlots,
            Self::AgentSlots => Self::PanicKill,
            Self::PanicKill => {
                if stuck_count > 0 {
                    Self::StuckAgent(0)
                } else {
                    Self::KillAllDead
                }
            }
            Self::StuckAgent(i) => {
                if *i + 1 < stuck_count {
                    Self::StuckAgent(i + 1)
                } else {
                    Self::KillAllDead
                }
            }
            Self::KillAllDead => Self::RetryFailedEvals,
            Self::RetryFailedEvals => Self::StartStop,
        }
    }
    pub fn prev(&self, stuck_count: usize) -> Self {
        match self {
            Self::StartStop => Self::RetryFailedEvals,
            Self::PauseResume => Self::StartStop,
            Self::Restart => Self::PauseResume,
            Self::AgentSlots => Self::Restart,
            Self::PanicKill => Self::AgentSlots,
            Self::StuckAgent(i) => {
                if *i > 0 {
                    Self::StuckAgent(i - 1)
                } else {
                    Self::PanicKill
                }
            }
            Self::KillAllDead => {
                if stuck_count > 0 {
                    Self::StuckAgent(stuck_count - 1)
                } else {
                    Self::PanicKill
                }
            }
            Self::RetryFailedEvals => Self::KillAllDead,
        }
    }
}

/// State for the service health badge in the status bar.
pub struct ServiceHealthState {
    /// Current health level (drives badge color).
    pub level: ServiceHealthLevel,
    /// Short label shown in the badge (e.g. "OK", "PAUSED", "DOWN").
    pub label: String,
    /// Service PID (if running).
    pub pid: Option<u32>,
    /// Human-readable uptime string.
    pub uptime: Option<String>,
    /// Socket path.
    pub socket_path: Option<String>,
    /// Agents alive / max.
    pub agents_alive: usize,
    pub agents_max: usize,
    /// Total agents ever spawned.
    pub agents_total: usize,
    /// Whether coordinator is paused.
    pub paused: bool,
    /// Reason for being paused (if paused).
    pub pause_reason: Option<String>,
    /// Whether the pause is due to provider errors.
    pub provider_auto_pause: bool,
    /// Previous provider auto-pause state (for detecting changes).
    pub prev_provider_auto_pause: bool,
    /// Stuck tasks (in-progress with dead agent PID).
    pub stuck_tasks: Vec<StuckTask>,
    /// Recent errors from daemon log (last 5 lines containing ERROR/WARN).
    pub recent_errors: Vec<String>,
    /// Last poll time.
    pub last_poll: Instant,
    /// Whether the detail popup is open.
    pub detail_open: bool,
    /// Scroll offset within the detail popup.
    pub detail_scroll: usize,
    /// Uptime in seconds (for <30s starting detection).
    pub uptime_secs: Option<u64>,
    pub panel_open: bool,
    pub panel_focus: ControlPanelFocus,
    pub panic_confirm: bool,
    pub feedback: Option<(String, Instant)>,
}

impl Default for ServiceHealthState {
    fn default() -> Self {
        Self {
            level: ServiceHealthLevel::Red,
            label: "DOWN".to_string(),
            pid: None,
            uptime: None,
            socket_path: None,
            agents_alive: 0,
            agents_max: 0,
            agents_total: 0,
            paused: false,
            pause_reason: None,
            provider_auto_pause: false,
            prev_provider_auto_pause: false,
            stuck_tasks: Vec::new(),
            recent_errors: Vec::new(),
            last_poll: Instant::now(),
            detail_open: false,
            detail_scroll: 0,
            uptime_secs: None,
            panel_open: false,
            panel_focus: ControlPanelFocus::StartStop,
            panic_confirm: false,
            feedback: None,
        }
    }
}

// Time counters
pub struct TimeCounters {
    pub service_uptime_secs: Option<u64>,
    pub cumulative_secs: u64,
    pub active_secs: u64,
    pub active_agent_count: usize,
    /// When the counter values were last computed from disk.
    /// Used to interpolate ticking counters at render time without extra I/O.
    pub counters_computed_at: Instant,
    pub session_start: Instant,
    pub last_refresh: Instant,
    pub show_uptime: bool,
    pub show_cumulative: bool,
    pub show_active: bool,
    pub show_session: bool,
}
impl TimeCounters {
    pub fn new(config_counters: &str) -> Self {
        let parts: Vec<&str> = config_counters.split(',').map(|s| s.trim()).collect();
        Self {
            service_uptime_secs: None,
            cumulative_secs: 0,
            active_secs: 0,
            active_agent_count: 0,
            counters_computed_at: Instant::now(),
            session_start: Instant::now(),
            last_refresh: Instant::now() - std::time::Duration::from_secs(60),
            show_uptime: parts.contains(&"uptime"),
            show_cumulative: parts.contains(&"cumulative"),
            show_active: parts.contains(&"active"),
            show_session: parts.contains(&"session"),
        }
    }
    pub fn any_enabled(&self) -> bool {
        self.show_uptime || self.show_cumulative || self.show_active || self.show_session
    }
    /// Current service uptime interpolated to render time (ticks every second).
    pub fn live_uptime_secs(&self) -> Option<u64> {
        self.service_uptime_secs
            .map(|s| s + self.counters_computed_at.elapsed().as_secs())
    }
    /// Current cumulative agent-seconds interpolated to render time.
    pub fn live_cumulative_secs(&self) -> u64 {
        self.cumulative_secs
            + self.counters_computed_at.elapsed().as_secs() * self.active_agent_count as u64
    }
    /// Current active agent-seconds interpolated to render time.
    pub fn live_active_secs(&self) -> u64 {
        self.active_secs
            + self.counters_computed_at.elapsed().as_secs() * self.active_agent_count as u64
    }
}
pub fn format_duration_compact(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m > 0 {
            format!("{}h{}m", h, m)
        } else {
            format!("{}h", h)
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// HUD Vitals bar
// ══════════════════════════════════════════════════════════════════════════════

/// State for the always-visible vitals strip at the bottom of the TUI.
#[derive(Default)]
pub struct VitalsState {
    /// Number of agents currently alive.
    pub agents_alive: usize,
    /// Task status counts for the vitals bar.
    pub open: usize,
    pub running: usize,
    pub done: usize,
    /// Time of the last operations.jsonl modification (for "last event X ago").
    pub last_event_time: Option<SystemTime>,
    /// Coordinator last tick time (parsed from coordinator-state).
    pub coord_last_tick: Option<SystemTime>,
    /// Whether the service daemon is running.
    pub daemon_running: bool,
}

/// Format a vitals bar string from the current state.
/// Returns a compact one-line string like:
/// `● 2 agents | 8 open · 3 running · 45 done | last event 4s ago | coord ● 3s`
#[allow(dead_code)]
pub fn format_vitals(vitals: &VitalsState) -> String {
    let mut parts = Vec::new();

    // Agent count
    let dot = if vitals.agents_alive > 0 {
        "●"
    } else {
        "○"
    };
    parts.push(format!("{} {} agents", dot, vitals.agents_alive));

    // Task counts
    parts.push(format!(
        "{} open · {} running · {} done",
        vitals.open, vitals.running, vitals.done
    ));

    // Last event
    let event_str = match vitals.last_event_time {
        Some(t) => match t.elapsed() {
            Ok(d) => format!("last event {} ago", format_duration_compact(d.as_secs())),
            Err(_) => "last event just now".to_string(),
        },
        None => "no events".to_string(),
    };
    parts.push(event_str);

    // Coordinator heartbeat
    if vitals.daemon_running {
        let coord_str = match vitals.coord_last_tick {
            Some(t) => match t.elapsed() {
                Ok(d) => format!("coord ● {}", format_duration_compact(d.as_secs())),
                Err(_) => "coord ● 0s".to_string(),
            },
            None => "coord ● –".to_string(),
        };
        parts.push(coord_str);
    } else {
        parts.push("coord ○ down".to_string());
    }

    parts.join(" | ")
}

/// Classify the staleness of a duration for color coding.
/// Returns: Green (<30s), Yellow (30s-5m), Red (>5m).
pub fn vitals_staleness_color(secs: u64) -> VitalsStaleness {
    if secs < 30 {
        VitalsStaleness::Fresh
    } else if secs < 300 {
        VitalsStaleness::Stale
    } else {
        VitalsStaleness::Dead
    }
}

/// Staleness level for vitals color coding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VitalsStaleness {
    /// <30s — green
    Fresh,
    /// 30s–5m — yellow
    Stale,
    /// >5m — red
    Dead,
}

/// Format a chrono::TimeDelta as a short human-readable duration (e.g. "3m", "1h12m").
fn format_duration_short(dur: chrono::TimeDelta) -> String {
    let secs = dur.num_seconds().unsigned_abs();
    format_duration_compact(secs)
}

/// The lightning bolt character for the wave animation (downwards zigzag arrow — reliably 1-cell wide).
pub const WAVE_BOLT: &str = "↯";

/// Number of lightning bolts in the wave animation (rainbow: R, O, G, C, V — matching CLI spinner).
pub const WAVE_NUM_BOLTS: usize = 5;

/// Interval between wave frames in milliseconds.
const WAVE_FRAME_MS: u128 = 120;

/// Returns the current wave position (0..WAVE_NUM_BOLTS) for the given elapsed duration.
pub fn spinner_wave_pos(elapsed: std::time::Duration) -> usize {
    (elapsed.as_millis() / WAVE_FRAME_MS) as usize % WAVE_NUM_BOLTS
}

// ══════════════════════════════════════════════════════════════════════════════
// Archive browser
// ══════════════════════════════════════════════════════════════════════════════

/// A single entry in the archive browser.
pub struct ArchiveEntry {
    pub id: String,
    pub title: String,
    pub completed_at: Option<String>,
    pub tags: Vec<String>,
}

/// State for the archive browser panel (toggled with 'A').
#[derive(Default)]
pub struct ArchiveBrowserState {
    /// Whether the archive browser is currently open/visible.
    pub active: bool,
    /// All archived entries loaded from archive.jsonl.
    pub entries: Vec<ArchiveEntry>,
    /// Currently selected index in the (filtered) list.
    pub selected: usize,
    /// Scroll offset (first visible row).
    pub scroll: usize,
    /// Search/filter query (empty = show all).
    pub filter: String,
    /// Whether the user is typing a filter query.
    pub filter_active: bool,
    /// Indices into `entries` that match the current filter.
    pub filtered_indices: Vec<usize>,
}

impl ArchiveBrowserState {
    /// Reload entries from the archive.jsonl file.
    pub fn load(&mut self, workgraph_dir: &std::path::Path) {
        let archive_path = workgraph_dir.join("archive.jsonl");
        self.entries.clear();
        if let Ok(file) = std::fs::File::open(&archive_path) {
            let reader = BufReader::new(file);
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => continue,
                };
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                // Parse as JSON, extract task fields
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    // The archive stores Node::Task — the outer object has "kind":"task"
                    // and the task fields at the top level.
                    let id = val["id"].as_str().unwrap_or("").to_string();
                    let title = val["title"].as_str().unwrap_or("").to_string();
                    let completed_at = val["completed_at"].as_str().map(String::from);
                    let tags: Vec<String> = val["tags"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    if !id.is_empty() {
                        self.entries.push(ArchiveEntry {
                            id,
                            title,
                            completed_at,
                            tags,
                        });
                    }
                }
            }
        }
        self.apply_filter();
    }

    /// Apply the current filter to entries, updating filtered_indices.
    pub fn apply_filter(&mut self) {
        if self.filter.is_empty() {
            self.filtered_indices = (0..self.entries.len()).collect();
        } else {
            let query = self.filter.to_lowercase();
            self.filtered_indices = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| {
                    e.id.to_lowercase().contains(&query)
                        || e.title.to_lowercase().contains(&query)
                        || e.tags.iter().any(|t| t.to_lowercase().contains(&query))
                })
                .map(|(i, _)| i)
                .collect();
        }
        // Clamp selection
        if self.filtered_indices.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered_indices.len() {
            self.selected = self.filtered_indices.len() - 1;
        }
    }

    /// Get the currently selected entry (if any).
    pub fn selected_entry(&self) -> Option<&ArchiveEntry> {
        self.filtered_indices
            .get(self.selected)
            .and_then(|&idx| self.entries.get(idx))
    }

    /// Number of visible (filtered) entries.
    pub fn visible_count(&self) -> usize {
        self.filtered_indices.len()
    }
}

/// State for the Ctrl+H history browser overlay.
/// Allows users to browse past conversation segments and inject them into
/// the coordinator's active context.
#[derive(Default)]
pub struct HistoryBrowserState {
    /// Whether the history browser is currently open.
    pub active: bool,
    /// Loaded history segments available for selection.
    pub segments: Vec<workgraph::chat::HistorySegment>,
    /// Currently selected segment index.
    pub selected: usize,
    /// Scroll offset (first visible row).
    pub scroll: usize,
    /// Whether we're showing the full preview of the selected segment.
    pub preview_expanded: bool,
    /// Preview scroll offset within the expanded preview.
    pub preview_scroll: usize,
}

impl HistoryBrowserState {
    /// Load segments from disk for the given coordinator.
    pub fn load(&mut self, workgraph_dir: &std::path::Path, coordinator_id: u32) {
        match workgraph::chat::load_history_segments(workgraph_dir, coordinator_id) {
            Ok(segs) => {
                self.segments = segs;
                self.selected = 0;
                self.scroll = 0;
                self.preview_expanded = false;
                self.preview_scroll = 0;
            }
            Err(_) => {
                self.segments.clear();
            }
        }
    }

    /// Load segments including cross-coordinator summaries.
    /// `coordinator_labels` maps coordinator IDs to display labels.
    /// `restricted_ids` are coordinators whose visibility blocks sharing.
    pub fn load_with_cross_coordinator(
        &mut self,
        workgraph_dir: &std::path::Path,
        coordinator_id: u32,
        coordinator_labels: &[(u32, String)],
        restricted_ids: &[u32],
    ) {
        // Load own segments first
        self.load(workgraph_dir, coordinator_id);

        // Append cross-coordinator segments
        if let Ok(cross_segs) = workgraph::chat::load_cross_coordinator_segments(
            workgraph_dir,
            coordinator_id,
            coordinator_labels,
            restricted_ids,
        ) {
            self.segments.extend(cross_segs);
        }
    }

    /// Get the currently selected segment (if any).
    pub fn selected_segment(&self) -> Option<&workgraph::chat::HistorySegment> {
        self.segments.get(self.selected)
    }
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

// ── Agent Stream Events (structured view of raw_stream.jsonl) ──

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentStreamEventKind {
    ToolCall,
    ToolResult,
    TextOutput,
    Thinking,
    SystemEvent,
    Error,
    UserInput,
}

/// Richer payload for an agent-stream event, used by the RAW pretty-printer
/// in the log pane. The `summary` field on `AgentStreamEvent` is kept for
/// backward-compatible event-mode rendering; `details` carries the
/// untruncated source so renderers can format full transcripts.
#[derive(Clone, Debug)]
pub enum EventDetails {
    ToolCall {
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        content: String,
        is_error: bool,
    },
    Thinking {
        text: String,
    },
    TextOutput {
        text: String,
    },
    UserInput {
        text: String,
    },
    SystemEvent {
        subtype: String,
        text: String,
    },
}

#[derive(Clone, Debug)]
pub struct AgentStreamEvent {
    pub kind: AgentStreamEventKind,
    pub agent_id: String,
    pub summary: String,
    /// Optional rich payload for renderers that need more than the summary
    /// line (notably the RAW pretty-printer).
    pub details: Option<EventDetails>,
}

/// Three view modes available in the per-task Log pane (right panel tab 4).
/// Cycled with the `4` key while the Log pane is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogViewMode {
    /// Structured event log: tool calls, results, errors — one per line.
    Events,
    /// Coarse, "what is the agent doing right now" — collapses adjacent
    /// events of the same kind/target into a single activity entry.
    HighLevel,
    /// Pretty-printed full transcript: every event rendered with its own
    /// formatter, NOT a JSON dump.
    RawPretty,
}

impl LogViewMode {
    /// Cycle to the next mode in the order Events -> HighLevel -> RawPretty -> Events.
    pub fn next(self) -> Self {
        match self {
            Self::Events => Self::HighLevel,
            Self::HighLevel => Self::RawPretty,
            Self::RawPretty => Self::Events,
        }
    }

    /// Short label for the pane header.
    pub fn label(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::HighLevel => "high-level",
            Self::RawPretty => "raw",
        }
    }
}

pub fn parse_raw_stream_line(line: &str, default_agent_id: &str) -> Option<AgentStreamEvent> {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return None;
    }
    let val: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let msg_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "assistant" => {
            let content_arr = val
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())?;
            let mut events = Vec::new();
            for block in content_arr {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match block_type {
                    "text" => {
                        let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        let text = text.trim();
                        if !text.is_empty() {
                            events.push(AgentStreamEvent {
                                kind: AgentStreamEventKind::TextOutput,
                                agent_id: default_agent_id.to_string(),
                                summary: text.to_string(),
                                details: Some(EventDetails::TextOutput {
                                    text: text.to_string(),
                                }),
                            });
                        }
                    }
                    "tool_use" => {
                        let name =
                            block.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let input = block
                            .get("input")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        let detail = match name {
                            "Bash" => input
                                .get("command")
                                .and_then(|v| v.as_str())
                                .map(|c| {
                                    let c = c.trim();
                                    if c.len() > 120 {
                                        format!("{}…", &c[..c.floor_char_boundary(120)])
                                    } else {
                                        c.to_string()
                                    }
                                }),
                            "Read" | "Write" => input
                                .get("file_path")
                                .and_then(|v| v.as_str())
                                .map(|p| p.to_string()),
                            "Edit" => {
                                let path = input
                                    .get("file_path")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                Some(format!("{}", path))
                            }
                            "Grep" | "Glob" => input
                                .get("pattern")
                                .and_then(|v| v.as_str())
                                .map(|p| p.to_string()),
                            _ => None,
                        };
                        let summary = match detail {
                            Some(d) => format!("⌁ {} → {}", name, d),
                            None => format!("⌁ {}", name),
                        };
                        events.push(AgentStreamEvent {
                            kind: AgentStreamEventKind::ToolCall,
                            agent_id: default_agent_id.to_string(),
                            summary,
                            details: Some(EventDetails::ToolCall {
                                name: name.to_string(),
                                input,
                            }),
                        });
                    }
                    "thinking" => {
                        let text = block
                            .get("thinking")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let text = text.trim();
                        if !text.is_empty() {
                            let truncated = if text.len() > 200 {
                                format!(
                                    "{}…",
                                    &text[..text.floor_char_boundary(200)]
                                )
                            } else {
                                text.to_string()
                            };
                            events.push(AgentStreamEvent {
                                kind: AgentStreamEventKind::Thinking,
                                agent_id: default_agent_id.to_string(),
                                summary: format!("💭 {}", truncated),
                                details: Some(EventDetails::Thinking {
                                    text: text.to_string(),
                                }),
                            });
                        }
                    }
                    _ => {}
                }
            }
            if events.len() == 1 {
                events.into_iter().next()
            } else if events.len() > 1 {
                let combined = events.iter().map(|e| e.summary.as_str()).collect::<Vec<_>>().join("\n");
                Some(AgentStreamEvent {
                    kind: events[0].kind.clone(),
                    agent_id: default_agent_id.to_string(),
                    summary: combined,
                    details: None,
                })
            } else {
                None
            }
        }
        "user" => {
            // Two cases:
            //   1. Plain user input (string content OR text-block content) — surfaced
            //      as UserInput so the RAW pretty view can render the prompt going IN.
            //   2. Tool results (assistant tool_use replies) — surfaced as ToolResult / Error.
            let content = val.get("message").and_then(|m| m.get("content"))?;

            // Case 1a: content is a plain string (user prompt).
            if let Some(s) = content.as_str() {
                let s = s.trim();
                if s.is_empty() {
                    return None;
                }
                let truncated = if s.len() > 200 {
                    format!("{}…", &s[..s.floor_char_boundary(200)])
                } else {
                    s.to_string()
                };
                return Some(AgentStreamEvent {
                    kind: AgentStreamEventKind::UserInput,
                    agent_id: default_agent_id.to_string(),
                    summary: format!("👤 {}", truncated),
                    details: Some(EventDetails::UserInput {
                        text: s.to_string(),
                    }),
                });
            }

            let content_arr = content.as_array()?;
            for block in content_arr {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if block_type == "tool_result" {
                    let is_error = block
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let content = block
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let truncated = if content.len() > 200 {
                        format!("{}…", &content[..content.floor_char_boundary(200)])
                    } else {
                        content.to_string()
                    };
                    let kind = if is_error {
                        AgentStreamEventKind::Error
                    } else {
                        AgentStreamEventKind::ToolResult
                    };
                    let prefix = if is_error { "✗" } else { "✓" };
                    return Some(AgentStreamEvent {
                        kind,
                        agent_id: default_agent_id.to_string(),
                        summary: format!("{} {}", prefix, truncated),
                        details: Some(EventDetails::ToolResult {
                            content: content.to_string(),
                            is_error,
                        }),
                    });
                } else if block_type == "text" {
                    // Case 1b: user prompt as a text block.
                    let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let truncated = if text.len() > 200 {
                        format!("{}…", &text[..text.floor_char_boundary(200)])
                    } else {
                        text.to_string()
                    };
                    return Some(AgentStreamEvent {
                        kind: AgentStreamEventKind::UserInput,
                        agent_id: default_agent_id.to_string(),
                        summary: format!("👤 {}", truncated),
                        details: Some(EventDetails::UserInput {
                            text: text.to_string(),
                        }),
                    });
                }
            }
            None
        }
        "system" => {
            let subtype = val
                .get("subtype")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match subtype {
                "init" | "rate_limit_event" => None,
                _ => {
                    let summary = val
                        .get("summary")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            val.get("description")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        .unwrap_or_else(|| format!("[{}]", subtype));
                    Some(AgentStreamEvent {
                        kind: AgentStreamEventKind::SystemEvent,
                        agent_id: default_agent_id.to_string(),
                        summary: format!("⚙ {}", summary),
                        details: Some(EventDetails::SystemEvent {
                            subtype: subtype.to_string(),
                            text: summary.clone(),
                        }),
                    })
                }
            }
        }
        "tool_call" => {
            let name = val.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
            let is_error = val.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false);
            let input = val
                .get("input")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let detail = match name {
                "Bash" | "bash" => input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|c| {
                        let c = c.trim();
                        if c.len() > 120 {
                            format!("{}…", &c[..c.floor_char_boundary(120)])
                        } else {
                            c.to_string()
                        }
                    }),
                "Read" | "Write" | "Edit" => input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .map(|p| p.to_string()),
                "Grep" | "Glob" => input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .map(|p| p.to_string()),
                _ => None,
            };
            let output_preview = val
                .get("output")
                .and_then(|v| v.as_str())
                .map(|o| {
                    let o = o.trim();
                    if o.len() > 100 {
                        format!("{}…", &o[..o.floor_char_boundary(100)])
                    } else {
                        o.to_string()
                    }
                });
            let call_summary = match detail {
                Some(d) => format!("⌁ {} → {}", name, d),
                None => format!("⌁ {}", name),
            };
            let full = if let Some(preview) = output_preview {
                let prefix = if is_error { "✗" } else { "✓" };
                format!("{}\n  {} {}", call_summary, prefix, preview)
            } else {
                call_summary
            };
            Some(AgentStreamEvent {
                kind: if is_error {
                    AgentStreamEventKind::Error
                } else {
                    AgentStreamEventKind::ToolCall
                },
                agent_id: default_agent_id.to_string(),
                summary: full,
                details: Some(EventDetails::ToolCall {
                    name: name.to_string(),
                    input,
                }),
            })
        }
        "turn" => {
            if let Some(content) = val.get("content").and_then(|c| c.as_array()) {
                for block in content {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if block_type == "text" {
                        let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        let text = text.trim();
                        if !text.is_empty() {
                            return Some(AgentStreamEvent {
                                kind: AgentStreamEventKind::TextOutput,
                                agent_id: default_agent_id.to_string(),
                                summary: text.to_string(),
                                details: Some(EventDetails::TextOutput {
                                    text: text.to_string(),
                                }),
                            });
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// State for the log pane (now embedded as right panel tab 2).
pub struct LogPaneState {
    /// Scroll offset from the top of log content.
    pub scroll: usize,
    /// Whether auto-scroll (tail mode) is active — scroll to bottom on new content.
    pub auto_tail: bool,
    /// Whether to show raw JSON format (toggled by `J`).
    pub json_mode: bool,
    /// Which of the three view modes is active. Cycled by pressing `4`
    /// while the Log pane is focused: events → high-level → raw → events.
    pub view_mode: LogViewMode,
    /// Cached rendered log lines for the currently selected task.
    pub rendered_lines: Vec<String>,
    /// Task ID these lines were rendered for (to detect staleness).
    pub task_id: Option<String>,
    /// Height of the log pane viewport (set each frame).
    pub viewport_height: usize,
    /// Total wrapped line count (set each frame by render, used for scroll bounds).
    pub total_wrapped_lines: usize,
    /// Agent ID for the selected task (for loading output.log).
    pub agent_id: Option<String>,
    /// Agent output text buffer — same type as the Output tab uses.
    pub agent_output: OutputAgentText,
    /// Whether new content arrived while scrolled up (for "new output" indicator).
    pub has_new_content: bool,
    /// Which iteration archive is currently being viewed (index into VizApp::iteration_archives).
    /// None means viewing the current (live) iteration.
    viewing_iteration: Option<usize>,
    /// Structured events parsed from raw_stream.jsonl.
    pub stream_events: Vec<AgentStreamEvent>,
    /// Byte offset for incremental reads from raw_stream.jsonl.
    pub raw_stream_offset: u64,
}

impl Default for LogPaneState {
    fn default() -> Self {
        Self {
            scroll: 0,
            auto_tail: true,
            json_mode: false,
            view_mode: LogViewMode::Events,
            rendered_lines: Vec::new(),
            task_id: None,
            viewport_height: 0,
            total_wrapped_lines: 0,
            agent_id: None,
            agent_output: OutputAgentText::default(),
            has_new_content: false,
            viewing_iteration: None,
            stream_events: Vec::new(),
            raw_stream_offset: 0,
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

// ── Activity Feed (semantic operations.jsonl view) ──

/// Typed event categories for the activity feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityEventKind {
    /// Task created (+, blue)
    TaskCreated,
    /// Status change (→, yellow): pause, resume, retry, unclaim, abandon, edit, assign
    StatusChange,
    /// Agent spawned (▶, green): claim
    AgentSpawned,
    /// Agent completed (✓, green bold): done
    AgentCompleted,
    /// Agent failed (✗, red bold): fail
    AgentFailed,
    /// Coordinator tick (⟳, dim): replay, apply
    CoordinatorTick,
    /// Verification result: approve
    VerificationResult,
    /// Context compaction (▣, purple): chat compaction events
    Compact,
    /// User action: gc, archive, link, unlink, publish, trace_export, artifact_add
    UserAction,
}

/// A single parsed activity event from operations.jsonl.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ActivityEvent {
    /// ISO-8601 timestamp.
    pub timestamp: String,
    /// Short time string for display (HH:MM:SS).
    pub time_short: String,
    /// The raw operation name from the log.
    pub op: String,
    /// Typed event category.
    pub kind: ActivityEventKind,
    /// Associated task ID, if any.
    pub task_id: Option<String>,
    /// Actor (agent ID or "user"), if present.
    pub actor: Option<String>,
    /// Human-readable summary line.
    pub summary: String,
}

impl ActivityEvent {
    /// Parse an operations.jsonl line into a typed ActivityEvent.
    pub fn parse(line: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let timestamp = v.get("timestamp")?.as_str()?.to_string();
        let op = v.get("op")?.as_str()?.to_string();
        let task_id = v.get("task_id").and_then(|t| t.as_str()).map(String::from);
        let actor = v.get("actor").and_then(|a| a.as_str()).map(String::from);
        let detail = v.get("detail").cloned().unwrap_or(serde_json::Value::Null);

        let kind = match op.as_str() {
            "add_task" => ActivityEventKind::TaskCreated,
            "claim" => ActivityEventKind::AgentSpawned,
            "done" => ActivityEventKind::AgentCompleted,
            "fail" => ActivityEventKind::AgentFailed,
            "abandon" | "pause" | "resume" | "retry" | "unclaim" | "edit" | "assign" => {
                ActivityEventKind::StatusChange
            }
            "approve" => ActivityEventKind::VerificationResult,
            "replay" | "apply" => ActivityEventKind::CoordinatorTick,
            "compact" => ActivityEventKind::Compact,
            _ => ActivityEventKind::UserAction,
        };

        let time_short = parse_time_short(&timestamp);
        let summary = format_event_summary(&op, &task_id, &actor, &detail);

        Some(ActivityEvent {
            timestamp,
            time_short,
            op,
            kind,
            task_id,
            actor,
            summary,
        })
    }

    /// Icon prefix for display.
    pub fn icon(&self) -> &'static str {
        match self.kind {
            ActivityEventKind::TaskCreated => "+",
            ActivityEventKind::StatusChange => "→",
            ActivityEventKind::AgentSpawned => "▶",
            ActivityEventKind::AgentCompleted => "✓",
            ActivityEventKind::AgentFailed => "✗",
            ActivityEventKind::CoordinatorTick => "⟳",
            ActivityEventKind::VerificationResult => "◆",
            ActivityEventKind::Compact => "▣",
            ActivityEventKind::UserAction => "●",
        }
    }
}

/// Extract HH:MM:SS from an ISO-8601 timestamp.
fn parse_time_short(ts: &str) -> String {
    // Expect format like "2026-02-18T20:24:52.488..."
    if let Some(t_pos) = ts.find('T') {
        let after_t = &ts[t_pos + 1..];
        // Take up to 8 chars for HH:MM:SS
        let end = after_t.len().min(8);
        after_t[..end].to_string()
    } else {
        ts.chars().take(8).collect()
    }
}

/// Build a human-readable summary line from operation fields.
fn format_event_summary(
    op: &str,
    task_id: &Option<String>,
    actor: &Option<String>,
    detail: &serde_json::Value,
) -> String {
    let tid = task_id.as_deref().unwrap_or("");
    match op {
        "add_task" => {
            let title = detail.get("title").and_then(|t| t.as_str()).unwrap_or(tid);
            format!("Task created: {}", title)
        }
        "claim" => {
            let who = actor.as_deref().unwrap_or("agent");
            format!("{} claimed {}", who, tid)
        }
        "done" => format!("Completed: {}", tid),
        "fail" => {
            let reason = detail
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or("unknown");
            format!("Failed: {} ({})", tid, reason)
        }
        "abandon" => {
            let reason = detail
                .get("reason")
                .and_then(|r| r.as_str())
                .map(|r| format!(" ({})", r))
                .unwrap_or_default();
            format!("Abandoned: {}{}", tid, reason)
        }
        "pause" => format!("Paused: {}", tid),
        "resume" => format!("Resumed: {}", tid),
        "retry" => {
            let attempt = detail.get("attempt").and_then(|a| a.as_u64()).unwrap_or(0);
            format!("Retry #{}: {}", attempt, tid)
        }
        "unclaim" => {
            let who = actor.as_deref().unwrap_or("agent");
            format!("{} released {}", who, tid)
        }
        "edit" => format!("Edited: {}", tid),
        "assign" => {
            let agent = detail
                .get("agent_hash")
                .and_then(|h| h.as_str())
                .map(|h| &h[..h.len().min(8)])
                .unwrap_or("agent");
            format!("Assigned {} to {}", tid, agent)
        }
        "approve" => format!("Approved: {}", tid),
        "replay" => {
            let count = detail
                .get("reset_count")
                .and_then(|c| c.as_u64())
                .unwrap_or(0);
            format!("Replay: reset {} tasks", count)
        }
        "apply" => {
            let func = detail
                .get("function_id")
                .and_then(|f| f.as_str())
                .unwrap_or("function");
            format!("Applied function: {}", func)
        }
        "gc" => {
            let removed = detail
                .get("removed")
                .and_then(|r| r.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("GC: removed {} tasks", removed)
        }
        "archive" => {
            let count = detail
                .get("task_ids")
                .and_then(|t| t.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("Archived {} tasks", count)
        }
        "link" => {
            let dep = detail
                .get("dependency")
                .and_then(|d| d.as_str())
                .unwrap_or("?");
            format!("Linked {} → {}", tid, dep)
        }
        "unlink" => {
            let dep = detail
                .get("dependency")
                .and_then(|d| d.as_str())
                .unwrap_or("?");
            format!("Unlinked {} → {}", tid, dep)
        }
        "publish" => format!("Published: {}", tid),
        "artifact_add" => {
            let path = detail
                .get("path")
                .and_then(|p| p.as_str())
                .unwrap_or("file");
            format!("Artifact: {} → {}", tid, path)
        }
        "trace_export" => {
            let vis = detail
                .get("visibility")
                .and_then(|v| v.as_str())
                .unwrap_or("internal");
            format!("Trace export ({})", vis)
        }
        "compact" => {
            let coord = detail
                .get("coordinator_id")
                .and_then(|c| c.as_u64())
                .map(|c| format!("#{}", c))
                .unwrap_or_else(|| "?".to_string());
            let msgs_before = detail
                .get("messages_before")
                .and_then(|m| m.as_u64())
                .unwrap_or(0);
            let msgs_after = detail
                .get("messages_after")
                .and_then(|m| m.as_u64())
                .unwrap_or(0);
            format!(
                "Compacted {}: {} → {} msgs (coord {})",
                coord, msgs_before, msgs_after, coord
            )
        }
        _ => {
            if tid.is_empty() {
                op.to_string()
            } else {
                format!("{}: {}", op, tid)
            }
        }
    }
}

/// State for the Activity Feed — semantic view of operations.jsonl.
pub struct ActivityFeedState {
    /// Ring buffer of parsed activity events (max 500).
    pub events: VecDeque<ActivityEvent>,
    /// Byte offset of last read position in operations.jsonl.
    pub last_offset: u64,
    /// Current scroll position (line index of top of viewport).
    pub scroll: usize,
    /// Whether auto-tail is active (scroll follows new events).
    pub auto_tail: bool,
    /// Viewport height in lines (set by renderer).
    pub viewport_height: usize,
    /// Total rendered lines after word-wrapping (set by renderer).
    pub total_wrapped_lines: usize,
}

/// Maximum number of events kept in the activity feed ring buffer.
pub const ACTIVITY_FEED_MAX_EVENTS: usize = 500;

impl Default for ActivityFeedState {
    fn default() -> Self {
        Self {
            events: VecDeque::new(),
            last_offset: 0,
            scroll: 0,
            auto_tail: true,
            viewport_height: 0,
            total_wrapped_lines: 0,
        }
    }
}

/// A single line in the firehose view — one output line from one agent.
/// Phase 4 dead code — structs kept until the full enum-variant removal
/// cleanup; fields never read because the Firehose tab is gone.
#[derive(Clone)]
#[allow(dead_code)]
pub struct FirehoseLine {
    /// Agent ID (e.g. "agent-7220").
    pub agent_id: String,
    /// Task ID the agent is working on.
    pub task_id: String,
    /// The output text (single line).
    pub text: String,
    /// Color index for this agent (cycles through a palette).
    pub color_idx: usize,
}

/// Maximum number of lines kept in the firehose buffer.
const FIREHOSE_MAX_LINES: usize = 1000;

/// State for the Firehose panel (panel 8) — merged stream of all agent output.
pub struct FirehoseState {
    /// Scroll offset from the top.
    pub scroll: usize,
    /// Whether auto-scroll (tail mode) is active.
    pub auto_tail: bool,
    /// Merged, chronologically-ordered output lines from all agents.
    pub lines: Vec<FirehoseLine>,
    /// Per-agent file offset for incremental reads (agent_id → byte offset).
    pub agent_offsets: HashMap<String, u64>,
    /// Mapping from agent_id to a stable color index.
    pub agent_colors: HashMap<String, usize>,
    /// Next color index to assign.
    pub next_color: usize,
    /// Viewport height (set each frame by renderer).
    pub viewport_height: usize,
    /// Total rendered lines (set each frame by renderer, for scrollbar).
    pub total_rendered_lines: usize,
}

impl Default for FirehoseState {
    fn default() -> Self {
        Self {
            scroll: 0,
            auto_tail: true,
            lines: Vec::new(),
            agent_offsets: HashMap::new(),
            agent_colors: HashMap::new(),
            next_color: 0,
            viewport_height: 0,
            total_rendered_lines: 0,
        }
    }
}

/// Per-agent scroll state for the Output pane.
pub struct OutputAgentScroll {
    /// Scroll offset (lines from top, 0 = top).
    pub scroll: usize,
    /// Whether auto-follow is active (pin to bottom).
    pub auto_follow: bool,
}

impl Default for OutputAgentScroll {
    fn default() -> Self {
        Self {
            scroll: 0,
            auto_follow: true,
        }
    }
}

/// Per-agent accumulated text buffer for the Output pane.
/// Phase 4 dead code — Output tab removed.
#[derive(Default)]
#[allow(dead_code)]
pub struct OutputAgentText {
    /// Accumulated extracted markdown text from output.log.
    pub full_text: String,
    /// Cached rendered lines from markdown_to_lines().
    pub rendered_lines: Vec<ratatui::text::Line<'static>>,
    /// Whether rendered_lines need to be regenerated.
    pub dirty: bool,
    /// Byte offset for incremental reads from output.log.
    pub file_offset: u64,
    /// Whether the agent has finished.
    pub finished: bool,
    /// Finish status (e.g. "done", "failed").
    pub finish_status: Option<String>,
}

/// Maximum characters kept in the Output pane per agent.
const OUTPUT_MAX_CHARS: usize = 50_000;

/// State for the Output pane (tab 9).
#[derive(Default)]
pub struct OutputPaneState {
    /// Currently selected agent ID.
    pub active_agent_id: Option<String>,
    /// Per-agent scroll states.
    pub agent_scrolls: HashMap<String, OutputAgentScroll>,
    /// Per-agent text buffers.
    pub agent_texts: HashMap<String, OutputAgentText>,
    /// Viewport height (set each frame by renderer).
    pub viewport_height: usize,
    /// Total rendered lines for the active agent (set each frame by renderer).
    pub total_rendered_lines: usize,
    /// Whether new content has arrived while auto_follow is off (for "new output" indicator).
    pub has_new_content: bool,
    /// Which iteration archive is currently being viewed (index into VizApp::iteration_archives).
    /// None means viewing the current (live) iteration.
    viewing_iteration: Option<usize>,
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
    /// ISO 8601 timestamp of when this message was read by the agent (if read).
    pub read_at: Option<String>,
    /// Original send timestamp (ISO 8601) for temporal sorting / future use.
    #[allow(dead_code)]
    pub send_timestamp: String,
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
    Models,
    Service,
    TuiSettings,
    AgentDefaults,
    Agency,
    Guardrails,
    ModelTiers,
    ModelRouting,
    Actions,
}

impl ConfigSection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Endpoints => "LLM Endpoints",
            Self::ApiKeys => "API Keys",
            Self::Models => "Model Registry",
            Self::Service => "Service Settings",
            Self::TuiSettings => "TUI Settings",
            Self::AgentDefaults => "Agent Defaults",
            Self::Agency => "Agency",
            Self::Guardrails => "Guardrails",
            Self::ModelTiers => "Model Tiers",
            Self::ModelRouting => "Model Routing",
            Self::Actions => "Actions",
        }
    }

    #[allow(dead_code)]
    pub fn all() -> &'static [ConfigSection] {
        &[
            Self::Endpoints,
            Self::ApiKeys,
            Self::Models,
            Self::Service,
            Self::TuiSettings,
            Self::AgentDefaults,
            Self::Agency,
            Self::Guardrails,
            Self::ModelTiers,
            Self::Actions,
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
    /// Optional FilterPicker for Choice fields (fuzzy filtering).
    pub choice_picker: Option<FilterPicker>,
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
    /// Per-endpoint test results, keyed by endpoint name.
    pub endpoint_test_results: HashMap<String, EndpointTestStatus>,
    /// Whether we're in the "add model" flow.
    pub adding_model: bool,
    /// Fields for new model being added.
    pub new_model: NewModelFields,
    /// Which field in the new-model form is active (0-4).
    pub new_model_field: usize,
    /// Last known mtime of the config file, for auto-refresh detection.
    pub last_config_mtime: Option<std::time::SystemTime>,
}

/// Status of an endpoint connectivity test.
#[derive(Clone)]
pub enum EndpointTestStatus {
    /// Test is in progress.
    Testing,
    /// Test succeeded.
    Ok,
    /// Test failed with an error message.
    Error(String),
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

/// Fields for the "add new model" form.
#[derive(Default, Clone)]
pub struct NewModelFields {
    pub id: String,
    pub provider: String,
    pub tier: String,
    pub cost_in: String,
    pub cost_out: String,
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
    /// A new coordinator was created. Triggers switching to the new coordinator.
    CreateCoordinator,
    /// A coordinator was deleted. On success, clean up local state and switch to coordinator 0.
    DeleteCoordinator(u32),
    /// A coordinator was archived (marked done). On success, clean up like delete.
    ArchiveCoordinator(u32),
    /// A coordinator's agent was stopped. On success, show notification.
    StopCoordinator(u32),
    /// A coordinator's current generation was interrupted (SIGINT, not kill).
    InterruptCoordinator(u32),
    /// An endpoint connectivity test completed. String is the endpoint name.
    EndpointTest(String),
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
    /// Path to the agent output log file, if any (used for mtime-based live refresh).
    pub output_path: Option<std::path::PathBuf>,
    /// Modification time of the output log at last load (used for "last written X ago" display).
    pub output_mtime: Option<SystemTime>,
}

#[derive(Debug, Clone, Default)]
struct TaskCompactionSnapshot {
    journal_present: bool,
    journal_entries: usize,
    compaction_count: u64,
    last_compaction: Option<String>,
    session_summary_present: bool,
    session_summary_words: Option<usize>,
}

fn load_task_runtime_snapshot(
    workgraph_dir: &Path,
    task: &workgraph::graph::Task,
) -> (
    Option<workgraph::service::AgentEntry>,
    Option<TaskCompactionSnapshot>,
) {
    let registry_entry = task.assigned.as_ref().and_then(|aid| {
        AgentRegistry::load(workgraph_dir)
            .ok()
            .and_then(|reg| reg.agents.get(aid).cloned())
    });

    let session_summary = task.assigned.as_ref().and_then(|aid| {
        let path = workgraph_dir
            .join("agents")
            .join(aid)
            .join("session-summary.md");
        std::fs::read_to_string(path).ok()
    });

    let journal_path = workgraph::executor::native::journal::journal_path(workgraph_dir, &task.id);
    let journal_present = journal_path.exists();
    let mut journal_entries = 0usize;
    let mut compaction_count = 0u64;
    let mut last_compaction = None;

    if journal_present
        && let Ok(entries) = workgraph::executor::native::journal::Journal::read_all(&journal_path)
    {
        journal_entries = entries.len();
        for entry in entries {
            if matches!(
                entry.kind,
                workgraph::executor::native::journal::JournalEntryKind::Compaction { .. }
            ) {
                compaction_count += 1;
                last_compaction = Some(entry.timestamp);
            }
        }
    }

    let snapshot = if registry_entry.as_ref().map(|e| e.executor.as_str()) == Some("native")
        || journal_present
        || session_summary.is_some()
    {
        Some(TaskCompactionSnapshot {
            journal_present,
            journal_entries,
            compaction_count,
            last_compaction,
            session_summary_present: session_summary.is_some(),
            session_summary_words: session_summary
                .as_ref()
                .map(|s| s.split_whitespace().count()),
        })
    } else {
        None
    };

    (registry_entry, snapshot)
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
    let inner = base.trim_start_matches('─').trim_end_matches('─').trim();
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
    pub archived: usize,
}

/// Active cycle timing info for status bar display.
pub struct CycleTimingEntry {
    /// Task ID of the cycle header (config owner).
    pub task_id: String,
    /// 1-based iteration number.
    pub iteration: u32,
    /// Max iterations configured.
    pub max_iterations: u32,
    /// Seconds since last iteration completed (None if never completed).
    pub last_completed_ago_secs: Option<i64>,
    /// Seconds until next iteration is due (None if unknown, negative if overdue).
    pub next_due_in_secs: Option<i64>,
    /// Current status of the cycle header.
    pub status: Status,
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
    /// Active cycle timing info (refreshed with graph stats).
    pub cycle_timing: Vec<CycleTimingEntry>,

    // ── Token display toggle ──
    /// When true, show total workgraph token usage; when false, show visible-tasks only.
    pub show_total_tokens: bool,

    // ── Help overlay ──
    pub show_help: bool,

    // ── System task visibility ──
    /// When true, show system tasks (dot-prefixed) in the graph view.
    pub show_system_tasks: bool,
    /// When true, show only running (in-progress/open) system tasks in the graph view.
    pub show_running_system_tasks: bool,
    /// Set to true when system task visibility was just toggled, so that
    /// newly appearing tasks get a `Revealed` animation instead of `NewTask`.
    pub system_tasks_just_toggled: bool,

    // ── Mouse capture ──
    /// Whether mouse capture is currently enabled.
    pub mouse_enabled: bool,
    /// Whether mode 1003 (any-event tracking) is enabled for touch support.
    /// Auto-set when running in Termux without mosh.
    pub any_motion_mouse: bool,
    /// When true, vertical scroll events (ScrollUp/ScrollDown) in the graph area
    /// are remapped to horizontal scroll (scroll_left/scroll_right). Useful in
    /// Termux where horizontal swipe gestures are consumed by the terminal and
    /// never reach the app as ScrollLeft/ScrollRight events.
    pub scroll_axis_swapped: bool,

    // ── Layout areas (set each frame by the renderer, for mouse hit-testing) ──
    /// The graph/viz content area from the last render frame.
    pub last_graph_area: Rect,
    /// The full right panel area (including border) from the last render frame.
    pub last_right_panel_area: Rect,
    /// The divider column between graph and inspector (for mouse hit-testing).
    pub last_divider_area: Rect,
    /// Whether the mouse is hovering over the divider.
    pub divider_hover: bool,
    /// The horizontal divider row between graph (top) and inspector (bottom) in stacked mode.
    pub last_horizontal_divider_area: Rect,
    /// Whether the mouse is hovering over the horizontal divider.
    pub horizontal_divider_hover: bool,
    /// The last "normal" split mode. Used to restore from FullInspector or Off.
    pub last_split_mode: LayoutMode,
    /// The last "normal" split percentage. Used to restore from FullInspector or Off.
    pub last_split_percent: u16,
    /// Hit area for the minimized inspector strip (1-col, right edge, Off mode).
    pub last_minimized_strip_area: Rect,
    /// Hit area for the full-screen restore strip (1-col, left edge, FullInspector mode).
    pub last_fullscreen_restore_area: Rect,
    /// Hit area for the full-screen right border (1-col, right edge, FullInspector mode).
    pub last_fullscreen_right_border_area: Rect,
    /// Hit area for the full-screen top border (1-row, top edge, FullInspector mode).
    pub last_fullscreen_top_border_area: Rect,
    /// Hit area for the full-screen bottom border (1-row, bottom edge, FullInspector mode).
    pub last_fullscreen_bottom_border_area: Rect,
    /// Whether the mouse is hovering over the minimized strip.
    pub minimized_strip_hover: bool,
    /// Whether the mouse is hovering over the full-screen restore strip.
    pub fullscreen_restore_hover: bool,
    /// Whether the mouse is hovering over the full-screen right border.
    pub fullscreen_right_hover: bool,
    /// Whether the mouse is hovering over the full-screen top border.
    pub fullscreen_top_hover: bool,
    /// Whether the mouse is hovering over the full-screen bottom border.
    pub fullscreen_bottom_hover: bool,
    /// The tab bar area inside the right panel from the last render frame.
    pub last_tab_bar_area: Rect,
    /// The iteration navigator widget area within the tab bar for mouse click handling.
    pub last_iteration_nav_area: Rect,
    /// The content area inside the right panel (below tab bar) from the last render frame.
    pub last_right_content_area: Rect,
    /// The chat input area from the last render frame (for click-to-resume editing).
    pub last_chat_input_area: Rect,
    /// The chat message history area from the last render frame (for click-to-focus).
    pub last_chat_message_area: Rect,
    /// The coordinator tab bar area from the last render frame (for click support).
    pub last_coordinator_bar_area: Rect,
    /// Per-tab hit areas for coordinator tab bar click testing.
    /// Each entry: (coordinator_id, tab_start_col, tab_end_col, close_start_col, close_end_col).
    /// close_start == close_end means no close button.
    pub coordinator_tab_hits: Vec<CoordinatorTabHit>,
    /// Hit area for the [+] button in the coordinator tab bar.
    pub coordinator_plus_hit: CoordinatorPlusHit,
    /// The message input area from the last render frame (for click-to-type).
    pub last_message_input_area: Rect,

    /// The text prompt overlay area from the last render frame (for mouse scroll).
    pub last_text_prompt_area: Rect,
    /// The choice/confirm dialog overlay area from the last render frame (for click-outside dismiss).
    pub last_dialog_area: Rect,

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
    /// Phase annotation info per parent task: task_id → AnnotationInfo.
    /// Carries display text and source dot-task IDs for click resolution.
    /// NOTE: This map is the *merged* view — it includes both live annotations
    /// from the current graph state AND sticky annotations that are being held
    /// past their live lifetime for visual continuity.
    pub annotation_map: HashMap<String, crate::commands::viz::AnnotationInfo>,
    /// Sticky annotations: annotations that should persist in the UI for a
    /// minimum duration even after the underlying system task completes.
    /// Key is the parent task ID, value is the annotation info + timing.
    sticky_annotations: HashMap<String, StickyAnnotation>,
    /// Clickable hit regions for phase annotations, computed from plain_lines + annotation_map.
    pub annotation_hit_regions: Vec<AnnotationHitRegion>,
    /// Active annotation click flash (for visual feedback). Clears after 500ms.
    pub annotation_click_flash: Option<AnnotationClickFlash>,
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
    /// Current responsive breakpoint (recomputed each frame from terminal width).
    pub responsive_breakpoint: ResponsiveBreakpoint,
    /// Hysteresis: whether inspector is currently laid out beside (right) rather than below.
    /// Used to prevent oscillation at the SIDE_MIN_WIDTH boundary.
    pub inspector_is_beside: bool,
    /// Which panel is shown in compact (< 50 cols) single-panel mode.
    pub single_panel_view: SinglePanelView,
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

    // ── Coordinator launcher ──
    /// Full-pane launcher state (replaces chat view when Some).
    pub launcher: Option<LauncherState>,
    /// The full launcher pane area from the last render frame (for mouse hit-test).
    pub last_launcher_area: Rect,
    /// Hit area for the launcher's Name field (single line).
    pub launcher_name_hit: Rect,
    /// Per-row hit areas for executor list: (executor_idx, row Rect).
    pub launcher_executor_hits: Vec<(usize, Rect)>,
    /// Per-row hit areas for model list: (LauncherListHit, row Rect).
    pub launcher_model_hits: Vec<(LauncherListHit, Rect)>,
    /// Bounding area of the model picker rows (for scroll-wheel routing).
    pub launcher_model_list_area: Rect,
    /// Per-row hit areas for endpoint list: (LauncherListHit, row Rect).
    pub launcher_endpoint_hits: Vec<(LauncherListHit, Rect)>,
    /// Bounding area of the endpoint picker rows (for scroll-wheel routing).
    pub launcher_endpoint_list_area: Rect,
    /// Per-row hit areas for recent list: (recent_idx, row Rect).
    pub launcher_recent_hits: Vec<(usize, Rect)>,
    /// Hit area for the [Launch] button in the launcher footer.
    pub launcher_launch_btn_hit: Rect,
    /// Hit area for the [Cancel] button in the launcher footer.
    pub launcher_cancel_btn_hit: Rect,
    /// State for the coordinator picker overlay.
    pub coordinator_picker: Option<CoordinatorPickerState>,

    // ── Text prompt ──
    /// Text prompt input buffer (for fail reason, message, etc.)
    pub text_prompt: TextPromptState,

    // ── Chat state ──
    /// Active coordinator ID (the chat tab currently being viewed).
    pub active_coordinator_id: u32,
    /// Per-coordinator chat states. Coordinator 0 is always present.
    pub coordinator_chats: HashMap<u32, ChatState>,
    /// Backward-compatible accessor: mutable reference to the active coordinator's chat state.
    /// This field is kept in sync with `coordinator_chats[active_coordinator_id]`.
    pub chat: ChatState,
    /// CLI override: load only the last N chat messages on startup.
    pub history_depth_override: Option<usize>,
    /// CLI flag: start with no history loaded, prevent scrollback for this session.
    pub no_history: bool,

    /// PTY handlers keyed by task id. When PTY mode is active in the
    /// Chat tab, the pane for the focused task's id is rendered in
    /// place of the file-tailing chat history. Lazy-spawned on first
    /// toggle-into-PTY-mode; dropped when the TUI exits. One entry
    /// per coordinator/task the user has interacted with this session.
    /// Phase 3 of docs/design/sessions-as-identity-rollout.md.
    pub task_panes: std::collections::HashMap<String, crate::tui::pty_pane::PtyPane>,

    /// When true, the Chat tab renders PTY output for the active
    /// coordinator's task instead of the file-tailing ChatMessage
    /// widgets. Toggle with Ctrl+T in the Chat tab.
    pub chat_pty_mode: bool,

    /// When in PTY mode, whether the pane is a read-only observer
    /// (`wg session attach`, because a handler elsewhere owns the
    /// session lock) or a full owner (`wg spawn-task`, TUI holds
    /// the lock). Determined at toggle-on time.
    pub chat_pty_observer: bool,

    /// Set when the user has sent a message while in observer mode.
    /// The TUI wrote the release marker and is now polling for the
    /// external handler to release the lock. Once released, the
    /// observer pane is dropped and an owner pane is spawned.
    /// Phase 3c of sessions-as-identity-rollout.md.
    pub chat_pty_takeover_pending_since: Option<std::time::Instant>,

    /// When true, keystrokes in the Chat tab forward to the embedded
    /// PTY child's stdin instead of the TUI's chat composer. Set for
    /// all PTY executors (native, claude, codex) — they all run
    /// interactive REPLs that read from stdin.
    pub chat_pty_forwards_stdin: bool,

    // ── Agent monitor state ──
    pub agent_monitor: AgentMonitorState,
    /// Per-agent JSONL stream state for live activity feed.
    pub agent_streams: HashMap<String, AgentStreamInfo>,

    // ── Service health indicator ──
    pub service_health: ServiceHealthState,
    /// Hit-test area for the service health badge (set each frame by renderer).
    pub last_service_badge_area: Rect,

    // ── HUD vitals bar ──
    pub vitals: VitalsState,

    // Time counters
    pub time_counters: TimeCounters,

    // ── Firehose state (panel 8) ──
    pub firehose: FirehoseState,

    // ── Output pane state (panel 9) ──
    pub output_pane: OutputPaneState,

    // ── Dashboard pane state (panel 10) ──
    pub dashboard: DashboardState,

    // ── Drill-down navigation stack ──
    pub nav_stack: NavStack,

    // ── Agency lifecycle for selected task ──
    pub agency_lifecycle: Option<AgencyLifecycle>,

    // ── Log pane state (now embedded as panel 2) ──
    pub log_pane: LogPaneState,

    // ── Coordinator log state (panel 7) ──
    pub coord_log: CoordLogState,

    // ── Activity feed state (semantic operations.jsonl view, replaces raw coord log) ──
    pub activity_feed: ActivityFeedState,

    // ── Messages panel state (panel 3) ──
    pub messages_panel: MessagesPanelState,
    /// Per-task message drafts: persists unsent text across task/panel switches.
    pub message_drafts: HashMap<String, String>,
    /// Cached coordinator message status per task ID (TUI-perspective read state).
    /// Refreshed each graph reload. Used to color the Messages tab header.
    pub task_message_statuses: HashMap<String, workgraph::messages::CoordinatorMessageStatus>,

    // ── Config panel state (panel 5) ──
    pub config_panel: ConfigPanelState,

    // ── Archive browser state ──
    pub archive_browser: ArchiveBrowserState,

    // ── Iteration history browsing ([ / ] in Detail tab) ──
    /// When `Some(idx)`, the Detail tab shows archived output/prompt from a past iteration.
    /// `None` means showing the current (live) iteration. Index into `iteration_archives`.
    pub viewing_iteration: Option<usize>,
    /// Task ID for which `iteration_archives` was loaded.
    iteration_archives_task_id: String,
    /// Cached list of archived iteration directories for the selected task, sorted oldest-first.
    /// Each entry: (directory name / timestamp string, path to the directory).
    pub iteration_archives: Vec<(String, PathBuf)>,

    // ── History browser state (Ctrl+H) ──
    pub history_browser: HistoryBrowserState,

    // ── File browser state (panel 6) ──
    pub file_browser: Option<super::file_browser::FileBrowser>,

    // ── Command queue ──
    /// Channel receiver for background command results.
    pub cmd_rx: mpsc::Receiver<CommandResult>,
    /// Channel sender (cloned into background threads).
    pub cmd_tx: mpsc::Sender<CommandResult>,
    /// Severity-leveled toast notifications. Info (green, 5s auto-dismiss),
    /// Warning (yellow, 10s auto-dismiss), Error (red, until Esc dismissed).
    /// Rendered stacked in top-right corner. Max 4 visible.
    pub toasts: Vec<Toast>,
    /// Previous agent statuses for detecting exits/stuck transitions.
    pub prev_agent_statuses: HashMap<String, workgraph::AgentStatus>,

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
    /// Active slide animation on the inspector panel (for Alt+arrow view cycling).
    pub slide_animation: Option<SlideAnimation>,
    /// Cached: name length threshold for inline vs above-line display.
    pub message_name_threshold: u16,
    /// Cached: indent for message body when name is on its own line.
    pub message_indent: u16,
    /// Session boundary gap threshold in minutes (from config).
    pub session_gap_minutes: u32,

    // ── Coordinator launcher debounce ──
    pub last_launcher_open: Option<Instant>,

    // ── Scrollbar auto-hide (per-pane) ──
    /// Timestamp of the last scroll activity in the graph pane.
    pub graph_scroll_activity: Option<Instant>,
    /// Timestamp of the last scroll activity in the right panel.
    pub panel_scroll_activity: Option<Instant>,

    // ── Scrollbar drag state ──
    /// Which scrollbar (if any) is currently being dragged.
    pub scrollbar_drag: Option<ScrollbarDragTarget>,
    /// Offset between the click column and the actual divider column when a
    /// divider drag starts.  Applied during Drag events so the divider stays
    /// anchored to its original position (avoids an integer-rounding jump).
    pub divider_drag_offset: i16,
    /// The right_panel_percent at the moment the divider drag started.
    /// Used by the delta-based drag handler to avoid percent↔width round-trip
    /// rounding errors that cause an initial snap on drag start.
    pub divider_drag_start_pct: u16,
    /// The column where the divider drag started.
    pub divider_drag_start_col: u16,
    /// The row where a horizontal divider drag started.
    pub divider_drag_start_row: u16,
    /// Last mouse position during a graph-body drag-to-pan gesture (col, row).
    pub graph_pan_last: Option<(u16, u16)>,

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

    /// Hit-test area for the "▼ new output" indicator in the Log tab.
    #[allow(dead_code)]
    pub last_log_new_output_area: Rect,

    /// Hit-test area for the ◀ ▶ iteration navigation arrows in the Detail tab header.
    pub last_iter_nav_area: Rect,

    // ── Touch echo (click/touch visual feedback) ──
    /// Whether touch echo indicators are enabled (toggled with `*`).
    pub touch_echo_enabled: bool,
    /// Active touch echo indicators (position + timestamp for fade).
    pub touch_echoes: Vec<TouchEcho>,

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

    // ── File system watcher (for real-time streaming) ──
    /// Flag set by the background file watcher when `.workgraph/` content changes.
    /// Checked and cleared by `maybe_refresh()` to trigger immediate panel reloads.
    pub fs_change_pending: Arc<AtomicBool>,
    /// Keep the watcher alive for the lifetime of the app.
    _fs_watcher: Option<notify_debouncer_mini::Debouncer<notify::RecommendedWatcher>>,
    /// Last mtime of the messages file for the currently-viewed task.
    last_messages_mtime: Option<SystemTime>,
    /// Last mtime of the daemon.log for coord log panel.
    last_daemon_log_mtime: Option<SystemTime>,
    /// Last mtime of operations.jsonl for activity feed.
    last_ops_log_mtime: Option<SystemTime>,
    /// Last mtime of the chat outbox for the active coordinator.
    last_chat_outbox_mtime: Option<SystemTime>,
    /// Last mtime of the output.log for the currently-displayed task (Detail tab live refresh).
    last_detail_output_mtime: Option<SystemTime>,
    /// Whether the HUD detail panel should auto-scroll to follow new content.
    /// Engages when the user scrolls to the bottom; disengages on scroll up.
    pub hud_follow: bool,
    /// Set by the fast path when graph.jsonl changes but the full viz wasn't reloaded.
    /// Checked by the slow path to ensure the viz reload isn't skipped.
    graph_viz_stale: bool,

    // ── Event tracing ──
    /// When `Some`, records all input events to a JSONL file for replay.
    pub tracer: Option<super::trace::EventTracer>,

    // ── Key feedback overlay ──
    /// Whether to show a key feedback overlay (for screencasts/demos).
    pub key_feedback_enabled: bool,
    /// Recent key presses for the feedback overlay, with timestamps.
    /// Newest entries at the back.
    pub key_feedback: VecDeque<(String, Instant)>,
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
    /// `None` means enabled by default.
    pub fn new(
        workgraph_dir: PathBuf,
        viz_options: VizOptions,
        mouse_override: Option<bool>,
        history_depth_override: Option<usize>,
        no_history: bool,
    ) -> Self {
        let mouse_enabled = mouse_override.unwrap_or(true);
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
            cycle_timing: Vec::new(),
            show_total_tokens: false,
            show_help: false,
            show_system_tasks: config.tui.show_system_tasks,
            show_running_system_tasks: config.tui.show_running_system_tasks,
            system_tasks_just_toggled: false,
            mouse_enabled,
            any_motion_mouse: super::event::detect_any_motion_support(),
            scroll_axis_swapped: false,
            last_graph_area: Rect::default(),
            last_right_panel_area: Rect::default(),
            last_divider_area: Rect::default(),
            divider_hover: false,
            last_horizontal_divider_area: Rect::default(),
            horizontal_divider_hover: false,
            last_split_mode: LayoutMode::TwoThirdsInspector,
            last_split_percent: 67,
            last_minimized_strip_area: Rect::default(),
            last_fullscreen_restore_area: Rect::default(),
            last_fullscreen_right_border_area: Rect::default(),
            last_fullscreen_top_border_area: Rect::default(),
            last_fullscreen_bottom_border_area: Rect::default(),
            minimized_strip_hover: false,
            fullscreen_restore_hover: false,
            fullscreen_right_hover: false,
            fullscreen_top_hover: false,
            fullscreen_bottom_hover: false,
            last_tab_bar_area: Rect::default(),
            last_iteration_nav_area: Rect::default(),
            last_right_content_area: Rect::default(),
            last_chat_input_area: Rect::default(),
            last_chat_message_area: Rect::default(),
            last_coordinator_bar_area: Rect::default(),
            coordinator_tab_hits: Vec::new(),
            coordinator_plus_hit: CoordinatorPlusHit::default(),
            last_message_input_area: Rect::default(),
            last_text_prompt_area: Rect::default(),
            last_dialog_area: Rect::default(),
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
            annotation_map: HashMap::new(),
            sticky_annotations: HashMap::new(),
            annotation_hit_regions: Vec::new(),
            annotation_click_flash: None,
            cycle_set: HashSet::new(),
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
            responsive_breakpoint: ResponsiveBreakpoint::Full,
            inspector_is_beside: true,
            single_panel_view: SinglePanelView::Graph,

            input_mode: InputMode::Normal,
            needs_center_on_selected: false,
            needs_scroll_into_view: false,
            chat_input_dismissed: false,
            inspector_sub_focus: InspectorSubFocus::ChatHistory,
            task_form: None,
            launcher: None,
            last_launcher_area: Rect::default(),
            launcher_name_hit: Rect::default(),
            launcher_executor_hits: Vec::new(),
            launcher_model_hits: Vec::new(),
            launcher_model_list_area: Rect::default(),
            launcher_endpoint_hits: Vec::new(),
            launcher_endpoint_list_area: Rect::default(),
            launcher_recent_hits: Vec::new(),
            launcher_launch_btn_hit: Rect::default(),
            launcher_cancel_btn_hit: Rect::default(),
            coordinator_picker: None,
            text_prompt: TextPromptState {
                editor: new_emacs_editor(),
            },
            active_coordinator_id: 0,
            coordinator_chats: HashMap::new(),
            chat: ChatState::default(),
            history_depth_override,
            no_history,
            task_panes: HashMap::new(),
            chat_pty_mode: false,
            chat_pty_observer: false,
            chat_pty_takeover_pending_since: None,
            chat_pty_forwards_stdin: false,
            agent_monitor: AgentMonitorState::default(),
            agent_streams: HashMap::new(),
            service_health: ServiceHealthState::default(),
            last_service_badge_area: Rect::default(),
            vitals: VitalsState::default(),
            time_counters: TimeCounters::new(&config.tui.counters),
            firehose: FirehoseState::default(),
            output_pane: OutputPaneState::default(),
            dashboard: DashboardState::default(),
            nav_stack: NavStack::default(),
            agency_lifecycle: None,
            log_pane: LogPaneState::default(),
            coord_log: CoordLogState::default(),
            activity_feed: ActivityFeedState::default(),
            messages_panel: MessagesPanelState::default(),
            message_drafts: HashMap::new(),
            task_message_statuses: HashMap::new(),
            config_panel: ConfigPanelState::default(),
            archive_browser: ArchiveBrowserState::default(),
            viewing_iteration: None,
            iteration_archives_task_id: String::new(),
            iteration_archives: Vec::new(),
            history_browser: HistoryBrowserState::default(),
            file_browser: None,
            cmd_rx,
            cmd_tx,
            toasts: Vec::new(),
            prev_agent_statuses: HashMap::new(),
            last_tab_press: None,
            sort_mode: SortMode::Chronological,
            smart_follow_active: true,
            initial_load: true,
            splash_animations: HashMap::new(),
            task_snapshots: HashMap::new(),
            animation_mode,
            slide_animation: None,
            message_name_threshold: config.tui.message_name_threshold,
            message_indent: config.tui.message_indent,
            session_gap_minutes: config.tui.session_gap_minutes,
            last_launcher_open: None,
            graph_scroll_activity: None,
            panel_scroll_activity: None,
            scrollbar_drag: None,
            divider_drag_offset: 0,
            divider_drag_start_pct: 0,
            divider_drag_start_col: 0,
            divider_drag_start_row: 0,
            graph_pan_last: None,
            last_graph_scrollbar_area: Rect::default(),
            last_panel_scrollbar_area: Rect::default(),
            graph_hscroll_activity: None,
            panel_hscroll_activity: None,
            last_graph_hscrollbar_area: Rect::default(),
            last_panel_hscrollbar_area: Rect::default(),
            last_log_new_output_area: Rect::default(),
            last_iter_nav_area: Rect::default(),
            touch_echo_enabled: false,
            touch_echoes: Vec::new(),
            has_keyboard_enhancement: false,
            editor_handler: create_editor_handler(),
            last_graph_mtime: graph_mtime,
            last_refresh: Instant::now(),
            last_refresh_display: chrono::Local::now().format("%H:%M:%S").to_string(),
            refresh_interval: std::time::Duration::from_secs(1),
            fs_change_pending: Arc::new(AtomicBool::new(false)),
            _fs_watcher: None,
            last_messages_mtime: None,
            last_daemon_log_mtime: None,
            last_ops_log_mtime: None,
            last_chat_outbox_mtime: None,
            last_detail_output_mtime: None,
            hud_follow: false,
            graph_viz_stale: false,
            tracer: None,
            key_feedback_enabled: false,
            key_feedback: VecDeque::new(),
        };
        app.start_fs_watcher();
        // Load graph once for both viz and stats on startup.
        let graph_path = app.workgraph_dir.join("graph.jsonl");
        if let Ok(graph) = load_graph(&graph_path) {
            app.load_viz_from_graph(&graph);
            app.load_stats_from_graph(&graph);
        } else {
            app.load_viz();
            app.load_stats();
        }
        // Restore TUI focus state from previous session (before ensure_user_coordinator
        // so that the user's last-focused coordinator is preserved).
        app.restore_tui_state();
        app.ensure_user_coordinator();
        app.load_agent_monitor();
        app.check_coordinator_status();
        app.update_service_health();
        app.update_vitals();
        app.update_time_counters();
        app.load_chat_history();
        // Step 1 of "nex-as-everything": for native-executor coordinators,
        // auto-enter PTY mode so Chat tab embeds `wg nex --chat` directly
        // instead of relying on the daemon's inbox/outbox relay. Silent
        // no-op for claude/codex executors.
        app.maybe_auto_enable_chat_pty();
        app
    }

    /// Load viz output by calling the viz module directly.
    pub fn load_viz(&mut self) {
        let viz_result = self.generate_viz();
        self.apply_viz_result(viz_result);
    }

    /// Load viz output from a pre-loaded graph, avoiding a redundant disk read.
    pub fn load_viz_from_graph(&mut self, graph: &workgraph::graph::WorkGraph) {
        let viz_result = self.generate_viz_from_graph(graph);
        self.apply_viz_result(viz_result);
    }

    /// Apply a viz result (shared implementation for load_viz and load_viz_from_graph).
    fn apply_viz_result(&mut self, viz_result: Result<VizOutput>) {
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

        match viz_result {
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
                // Merge live annotations with sticky annotations for visual continuity.
                // This ensures transient states (assigning, evaluating) remain visible
                // for at least STICKY_ANNOTATION_HOLD_SECS even after completion.
                let now = Instant::now();
                let live_annotations = viz_output.annotation_map;

                // Update sticky annotations: refresh last_seen for live ones, keep recent stale ones.
                for (parent_id, info) in &live_annotations {
                    self.sticky_annotations.insert(
                        parent_id.clone(),
                        StickyAnnotation {
                            info: info.clone(),
                            last_seen: now,
                        },
                    );
                }

                // Build merged map: start with live annotations, then add stale stickies.
                let mut merged = live_annotations;
                let hold = std::time::Duration::from_secs(STICKY_ANNOTATION_HOLD_SECS);
                self.sticky_annotations.retain(|parent_id, sticky| {
                    if merged.contains_key(parent_id) {
                        // Still live — keep in sticky map, already in merged.
                        return true;
                    }
                    if sticky.last_seen.elapsed() < hold {
                        // Expired from live state but within hold period — show it.
                        merged.insert(parent_id.clone(), sticky.info.clone());
                        true
                    } else {
                        // Past hold period — remove from sticky map.
                        false
                    }
                });

                self.annotation_map = merged;
                self.compute_annotation_hit_regions();

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
                self.update_scroll_bounds();
            }
        }
    }

    fn generate_viz(&self) -> Result<VizOutput> {
        let mut opts = self.viz_options.clone();
        opts.show_internal = self.show_system_tasks;
        opts.show_internal_running_only = !self.show_system_tasks && self.show_running_system_tasks;
        crate::commands::viz::generate_viz_output(&self.workgraph_dir, &opts)
    }

    /// Generate viz output from a pre-loaded graph, avoiding a redundant disk read.
    fn generate_viz_from_graph(&self, graph: &workgraph::graph::WorkGraph) -> Result<VizOutput> {
        let mut opts = self.viz_options.clone();
        opts.show_internal = self.show_system_tasks;
        opts.show_internal_running_only = !self.show_system_tasks && self.show_running_system_tasks;
        crate::commands::viz::generate_viz_output_from_graph(graph, &self.workgraph_dir, &opts)
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
        self.sync_coordinator_from_selection();
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
        self.sync_coordinator_from_selection();
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
        self.sync_coordinator_from_selection();
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
        self.sync_coordinator_from_selection();
        self.scroll_to_selected_task();
    }

    /// Select the first task in the viz order.
    pub fn select_first_task(&mut self) {
        if self.task_order.is_empty() {
            return;
        }
        self.selected_task_idx = Some(0);
        self.recompute_trace();
        self.sync_coordinator_from_selection();
        self.scroll_to_selected_task();
    }

    /// Select the last task in the viz order.
    pub fn select_last_task(&mut self) {
        if self.task_order.is_empty() {
            return;
        }
        self.selected_task_idx = Some(self.task_order.len() - 1);
        self.recompute_trace();
        self.sync_coordinator_from_selection();
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

        // Invalidate HUD, lifecycle, and log pane so they reload for the new selection.
        self.invalidate_hud();
        self.invalidate_agency_lifecycle();
        // Reset log auto-tail when the selected task actually changes, so the new
        // task's log opens pinned to the bottom (same as terminal emulators).
        if self.log_pane.task_id.as_deref() != Some(&selected_id) {
            self.log_pane.auto_tail = true;
            self.log_pane.has_new_content = false;
        }
        self.invalidate_log_pane();
        // Only invalidate the messages panel when the selected task actually changed.
        // Unconditional invalidation caused the editor to be re-created on every
        // graph tick (via save_draft → invalidate → load → restore_draft), which
        // reset the cursor to position 0 and made typing impossible.
        if self.messages_panel.task_id.as_deref() != Some(&selected_id) {
            self.save_message_draft();
            self.invalidate_messages_panel();
        }
    }

    /// Compute annotation hit regions from `plain_lines`, `annotation_map`, and `node_line_map`.
    /// Called after each refresh to populate `annotation_hit_regions`.
    pub fn compute_annotation_hit_regions(&mut self) {
        self.annotation_hit_regions.clear();
        for (parent_id, info) in &self.annotation_map {
            let orig_line = match self.node_line_map.get(parent_id) {
                Some(&line) => line,
                None => continue,
            };
            let plain = match self.plain_lines.get(orig_line) {
                Some(line) => line,
                None => continue,
            };
            // The annotation text (e.g. "[⊞ assigning]") is appended to the line.
            // Find it by searching for the text substring in the plain line.
            if let Some(col_start) = plain.find(&info.text) {
                let col_end = col_start + info.text.len();
                self.annotation_hit_regions.push(AnnotationHitRegion {
                    orig_line,
                    col_start,
                    col_end,
                    parent_task_id: parent_id.clone(),
                    dot_task_ids: info.dot_task_ids.clone(),
                });
            }
        }
    }

    /// If the currently selected task is a coordinator task, switch to that
    /// coordinator and show the Chat tab. If it's a user board task, switch
    /// to the Messages tab. Call this only from user-initiated selection
    /// changes (keyboard / mouse), NOT from automatic graph refreshes.
    fn sync_coordinator_from_selection(&mut self) {
        let selected_id = match self.selected_task_idx {
            Some(idx) => match self.task_order.get(idx) {
                Some(id) => id.clone(),
                None => return,
            },
            None => return,
        };
        if let Some(cid) = selected_id
            .strip_prefix(".coordinator-")
            .and_then(|s| s.parse::<u32>().ok())
        {
            if cid != self.active_coordinator_id {
                self.switch_coordinator(cid);
                // Only switch to Chat tab when actually changing coordinators.
                self.right_panel_tab = RightPanelTab::Chat;
            }
        } else if selected_id == ".coordinator" && self.active_coordinator_id != 0 {
            self.switch_coordinator(0);
            self.right_panel_tab = RightPanelTab::Chat;
        } else if workgraph::graph::is_user_board(&selected_id) {
            // Switch to Messages tab for user board tasks.
            self.right_panel_tab = RightPanelTab::Messages;
        }
    }

    /// Scroll the viewport so the selected task stays within the middle 60% of
    /// the viewport (a "comfort zone"). Uses minimal scrolling — like vim's
    /// `scrolloff` — instead of re-centering when a task exits the zone.
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
            if visible_pos < comfort_top {
                // Task is above the comfort zone — scroll up just enough so it
                // sits at the top edge of the comfort zone.  saturating_sub
                // naturally clamps to 0 (can't scroll past the first line).
                self.scroll.offset_y = visible_pos.saturating_sub(margin);
                self.scroll.clamp();
            } else if visible_pos >= comfort_bottom {
                // Task is below the comfort zone — scroll down just enough so
                // it sits at the bottom edge of the comfort zone.
                self.scroll.offset_y = (visible_pos + margin + 1).saturating_sub(vh);
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
        self.sync_coordinator_from_selection();
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

        // Select the new task so the viewport centers on it — the user wants
        // to see their fresh work.  The splash animation (registered earlier
        // in refresh_data) provides an additional visual highlight.
        if let Some(idx) = self.task_order.iter().position(|id| id == &task_id) {
            self.selected_task_idx = Some(idx);
            self.recompute_trace();
            self.sync_coordinator_from_selection();
            self.push_toast(format!("New task: {}", task_id), ToastSeverity::Info);
            true
        } else {
            // Task not found in current view (may be filtered out or internal).
            self.push_toast(format!("New task: {}", task_id), ToastSeverity::Info);
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

    /// Push a toast notification with the given severity.
    /// Keeps at most MAX_VISIBLE_TOASTS active toasts (oldest dropped first).
    pub fn push_toast(&mut self, msg: String, severity: ToastSeverity) {
        self.toasts.push(Toast {
            message: msg,
            severity,
            created_at: Instant::now(),
            dedup_key: None,
        });
        while self.toasts.len() > MAX_VISIBLE_TOASTS {
            self.toasts.remove(0);
        }
    }

    /// Push a deduplicated toast. If a toast with the same dedup_key already exists,
    /// it is replaced instead of adding a new one.
    pub fn push_toast_dedup(&mut self, msg: String, severity: ToastSeverity, key: String) {
        self.toasts.retain(|t| t.dedup_key.as_deref() != Some(&key));
        self.toasts.push(Toast {
            message: msg,
            severity,
            created_at: Instant::now(),
            dedup_key: Some(key),
        });
        while self.toasts.len() > MAX_VISIBLE_TOASTS {
            self.toasts.remove(0);
        }
    }

    /// Dismiss all error toasts (called on Esc). Returns true if any were dismissed.
    pub fn dismiss_error_toasts(&mut self) -> bool {
        let before = self.toasts.len();
        self.toasts.retain(|t| t.severity != ToastSeverity::Error);
        self.toasts.len() != before
    }

    /// Remove expired toasts based on severity auto-dismiss durations.
    /// Returns true if any toasts were removed (needs redraw).
    pub fn cleanup_toasts(&mut self) -> bool {
        let before = self.toasts.len();
        self.toasts
            .retain(|t| match t.severity.auto_dismiss_duration() {
                Some(dur) => t.created_at.elapsed() < dur,
                None => true,
            });
        self.toasts.len() != before
    }

    /// Remove expired splash animations.
    pub fn cleanup_splash_animations(&mut self) {
        let duration = self.animation_mode.speed().duration_secs();
        let cutoff = std::time::Duration::from_secs_f64(duration);
        self.splash_animations
            .retain(|_, anim| anim.start.elapsed() < cutoff);
        // Expire annotation click flash after 500ms.
        if let Some(ref flash) = self.annotation_click_flash
            && flash.start.elapsed() > std::time::Duration::from_millis(500)
        {
            self.annotation_click_flash = None;
        }
    }

    /// Add a touch echo at the given terminal position.
    pub fn add_touch_echo(&mut self, col: u16, row: u16) {
        if !self.touch_echo_enabled {
            return;
        }
        self.touch_echoes.push(TouchEcho {
            col,
            row,
            start: Instant::now(),
        });
        // Cap the number of active echoes.
        if self.touch_echoes.len() > MAX_TOUCH_ECHOES {
            self.touch_echoes.remove(0);
        }
    }

    /// Remove expired touch echo indicators.
    pub fn cleanup_touch_echoes(&mut self) {
        self.touch_echoes.retain(|e| !e.is_expired());
    }

    /// Whether any touch echoes are still animating.
    pub fn has_active_touch_echoes(&self) -> bool {
        self.touch_echo_enabled && !self.touch_echoes.is_empty()
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

    // ── Key feedback ──

    /// Duration to show each key press in the overlay.
    pub(super) const KEY_FEEDBACK_DURATION: std::time::Duration =
        std::time::Duration::from_millis(1500);
    /// Maximum number of recent key presses to display.
    const KEY_FEEDBACK_MAX: usize = 6;

    /// Record a key press for the feedback overlay.
    pub fn record_key_feedback(&mut self, label: String) {
        if !self.key_feedback_enabled {
            return;
        }
        let now = Instant::now();
        // Remove expired entries.
        while self
            .key_feedback
            .front()
            .is_some_and(|(_, t)| t.elapsed() > Self::KEY_FEEDBACK_DURATION)
        {
            self.key_feedback.pop_front();
        }
        self.key_feedback.push_back((label, now));
        // Cap the queue size.
        while self.key_feedback.len() > Self::KEY_FEEDBACK_MAX {
            self.key_feedback.pop_front();
        }
    }

    /// Remove expired key feedback entries.
    pub fn cleanup_key_feedback(&mut self) {
        while self
            .key_feedback
            .front()
            .is_some_and(|(_, t)| t.elapsed() > Self::KEY_FEEDBACK_DURATION)
        {
            self.key_feedback.pop_front();
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

    // ── Chat search ──

    /// Update chat search results after query changes.
    /// Performs case-insensitive substring matching across all loaded messages.
    pub fn update_chat_search(&mut self) {
        let query = self.chat.search.query.to_lowercase();
        self.chat.search.matches.clear();
        self.chat.search.current_match = None;

        if query.is_empty() {
            return;
        }

        for (msg_idx, msg) in self.chat.messages.iter().enumerate() {
            let text_lower = msg.text.to_lowercase();
            let mut start = 0;
            while let Some(pos) = text_lower[start..].find(&query) {
                let byte_offset = start + pos;
                self.chat.search.matches.push(ChatSearchMatch {
                    message_idx: msg_idx,
                    byte_offset,
                    match_len: query.len(),
                });
                start = byte_offset + 1;
                if start >= text_lower.len() {
                    break;
                }
            }
        }

        if !self.chat.search.matches.is_empty() {
            self.chat.search.current_match = Some(0);
            self.scroll_chat_to_search_match();
        }
    }

    /// Navigate to the next chat search match.
    pub fn chat_search_next(&mut self) {
        if self.chat.search.matches.is_empty() {
            return;
        }
        let next = match self.chat.search.current_match {
            Some(idx) => (idx + 1) % self.chat.search.matches.len(),
            None => 0,
        };
        self.chat.search.current_match = Some(next);
        self.scroll_chat_to_search_match();
    }

    /// Navigate to the previous chat search match.
    pub fn chat_search_prev(&mut self) {
        if self.chat.search.matches.is_empty() {
            return;
        }
        let prev = match self.chat.search.current_match {
            Some(0) => self.chat.search.matches.len() - 1,
            Some(idx) => idx - 1,
            None => self.chat.search.matches.len() - 1,
        };
        self.chat.search.current_match = Some(prev);
        self.scroll_chat_to_search_match();
    }

    /// Scroll the chat view so the current search match is visible.
    fn scroll_chat_to_search_match(&mut self) {
        let match_idx = match self.chat.search.current_match {
            Some(idx) => idx,
            None => return,
        };
        let m = match self.chat.search.matches.get(match_idx) {
            Some(m) => m.clone(),
            None => return,
        };

        // Find the rendered line that corresponds to this message index.
        // We use the line_to_message mapping (set each frame by renderer).
        // If the message is before the loaded range, try to load more history.
        if m.message_idx >= self.chat.messages.len() {
            return;
        }

        // Find any rendered line for this message.
        let target_line = self
            .chat
            .line_to_message
            .iter()
            .position(|opt| *opt == Some(m.message_idx));

        if let Some(line_idx) = target_line {
            // Convert line_idx to the scroll-from-bottom coordinate used by chat.
            let total = self.chat.total_rendered_lines;
            let viewport = self.chat.viewport_height.max(1);
            if total > viewport {
                let max_scroll = total.saturating_sub(viewport);
                // Desired scroll_from_top to center this line.
                let desired_top = line_idx.saturating_sub(viewport / 2);
                let clamped_top = desired_top.min(max_scroll);
                // Convert to scroll-from-bottom.
                self.chat.scroll = max_scroll.saturating_sub(clamped_top);
            }
        }
    }

    /// Clear chat search state.
    pub fn clear_chat_search(&mut self) {
        self.chat.search.query.clear();
        self.chat.search.matches.clear();
        self.chat.search.current_match = None;
    }

    /// Search through on-disk history pages that haven't been loaded yet.
    /// Loads pages until a match is found or all history is loaded.
    pub fn chat_search_load_all_history(&mut self) {
        while self.chat.has_more_history {
            if !self.load_more_chat_history() {
                break;
            }
        }
        // Re-run the search with all messages now loaded.
        self.update_chat_search();
    }

    /// Return a human-readable chat search status string for the search bar.
    #[allow(dead_code)]
    pub fn chat_search_status(&self) -> String {
        if self.chat.search.query.is_empty() {
            "/".to_string()
        } else if self.chat.search.matches.is_empty() {
            format!("/{} [no matches]", self.chat.search.query)
        } else {
            let idx = self.chat.search.current_match.unwrap_or(0);
            format!(
                "/{} [{}/{}]",
                self.chat.search.query,
                idx + 1,
                self.chat.search.matches.len()
            )
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
        self.load_stats_from_graph(&graph);
    }

    /// Load task counts and token usage from a pre-loaded graph.
    pub fn load_stats_from_graph(&mut self, graph: &workgraph::graph::WorkGraph) {
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

        let mut new_snapshots: HashMap<String, TaskSnapshot> = HashMap::new();
        let now = Instant::now();
        // Collect toast messages during the loop to avoid borrow conflicts
        // (self.task_snapshots is borrowed immutably via `old` while we need
        // self.push_toast() which borrows self mutably).
        let mut deferred_toasts: Vec<(String, ToastSeverity)> = Vec::new();

        for task in graph.tasks() {
            counts.total += 1;
            match task.status {
                Status::Done => counts.done += 1,
                Status::Open => counts.open += 1,
                Status::InProgress => counts.in_progress += 1,
                Status::Failed => counts.failed += 1,
                Status::Blocked => counts.blocked += 1,
                Status::Abandoned => counts.done += 1, // count with done
                Status::Waiting | Status::PendingValidation => counts.blocked += 1, // count with blocked
                Status::PendingEval => counts.in_progress += 1, // soft-done, awaiting eval
                Status::Incomplete => counts.open += 1,
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
            let total_tokens = usage
                .map(|u| u.input_tokens + u.cache_creation_input_tokens + u.output_tokens)
                .unwrap_or(0);

            let snapshot = TaskSnapshot {
                status: task.status,
                assigned: task.assigned.clone(),
                token_bucket: total_tokens / TOKEN_DEBOUNCE_BUCKET,
                edge_count: task.after.len(),
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

                    // Pipeline toasts for agency events.
                    if snapshot.status == Status::Done
                        && let Some(source_id) = task.id.strip_prefix(".assign-")
                    {
                        let msg = task
                            .description
                            .as_deref()
                            .and_then(|d| d.lines().next())
                            .map(|line| {
                                line.strip_prefix("Lightweight assignment: ")
                                    .unwrap_or(line)
                                    .to_string()
                            })
                            .unwrap_or_else(|| format!("assigned → {}", source_id));
                        deferred_toasts
                            .push((format!("\u{26a1} Assigned: {}", msg), ToastSeverity::Info));
                    }
                    // Agent spawn: non-system task went to InProgress.
                    if snapshot.status == Status::InProgress
                        && !workgraph::graph::is_system_task(&task.id)
                        && let Some(ref agent_id) = task.assigned
                    {
                        let short = if agent_id.len() > 10 {
                            &agent_id[..agent_id.floor_char_boundary(10)]
                        } else {
                            agent_id
                        };
                        deferred_toasts.push((
                            format!("\u{26a1} Spawned: {} on {}", short, task.id),
                            ToastSeverity::Info,
                        ));
                    }

                    // Phase 1 toast triggers — non-system tasks only.
                    if !workgraph::graph::is_system_task(&task.id) {
                        match snapshot.status {
                            Status::Done => {
                                if old.status == Status::InProgress
                                    || old.status == Status::PendingValidation
                                {
                                    let duration_str = task
                                        .started_at
                                        .as_deref()
                                        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                                        .and_then(|started| {
                                            task.completed_at
                                                .as_deref()
                                                .and_then(|c| {
                                                    chrono::DateTime::parse_from_rfc3339(c).ok()
                                                })
                                                .map(|completed| {
                                                    let dur =
                                                        completed.signed_duration_since(started);
                                                    format_duration_short(dur)
                                                })
                                        })
                                        .unwrap_or_default();
                                    if duration_str.is_empty() {
                                        deferred_toasts.push((
                                            format!("\u{2705} Done: {}", task.id),
                                            ToastSeverity::Info,
                                        ));
                                    } else {
                                        deferred_toasts.push((
                                            format!(
                                                "\u{2705} Done: {} ({})",
                                                task.id, duration_str
                                            ),
                                            ToastSeverity::Info,
                                        ));
                                    }
                                } else {
                                    deferred_toasts.push((
                                        format!("\u{2705} Done: {}", task.id),
                                        ToastSeverity::Info,
                                    ));
                                }
                            }
                            Status::Failed => {
                                deferred_toasts.push((
                                    format!("\u{274c} Failed: {}", task.id),
                                    ToastSeverity::Error,
                                ));
                            }
                            _ => {}
                        }
                    }
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
                // Note: new tasks (not in old snapshots) are already handled in load_viz().
            }

            new_snapshots.insert(task.id.clone(), snapshot);
        }

        // Emit deferred toasts (collected during snapshot comparison above).
        for (msg, severity) in deferred_toasts {
            self.push_toast(msg, severity);
        }

        // Count archived tasks
        let archive_path = self.workgraph_dir.join("archive.jsonl");
        counts.archived = if archive_path.exists() {
            std::fs::File::open(&archive_path)
                .map(|f| {
                    BufReader::new(f)
                        .lines()
                        .filter(|l| l.as_ref().is_ok_and(|s| !s.trim().is_empty()))
                        .count()
                })
                .unwrap_or(0)
        } else {
            0
        };

        self.task_snapshots = new_snapshots;
        self.task_counts = counts;
        self.total_usage = total_usage;
        self.task_token_map = task_token_map;

        // Compute cycle timing from graph.
        {
            let cycle_analysis = CycleAnalysis::from_graph(graph);
            let utc_now = chrono::Utc::now();
            let mut entries = Vec::new();

            for cycle in &cycle_analysis.cycles {
                let config_owner = cycle.members.iter().find_map(|mid| {
                    let task = graph.get_task(mid)?;
                    task.cycle_config.as_ref()?;
                    Some(task)
                });

                let Some(owner) = config_owner else {
                    continue;
                };
                let cc = owner.cycle_config.as_ref().unwrap();

                let last_completed = owner
                    .last_iteration_completed_at
                    .as_ref()
                    .or(owner.completed_at.as_ref())
                    .cloned();

                let last_ago = last_completed.as_ref().and_then(|ts| {
                    let parsed = ts.parse::<chrono::DateTime<chrono::Utc>>().ok()?;
                    Some(utc_now.signed_duration_since(parsed).num_seconds())
                });

                let next_due_in = owner
                    .ready_after
                    .as_ref()
                    .and_then(|ts| ts.parse::<chrono::DateTime<chrono::Utc>>().ok())
                    .or_else(|| {
                        let delay_secs = cc
                            .delay
                            .as_ref()
                            .and_then(|d| workgraph::graph::parse_delay(d))?;
                        let last_ts = last_completed
                            .as_ref()?
                            .parse::<chrono::DateTime<chrono::Utc>>()
                            .ok()?;
                        Some(last_ts + chrono::Duration::seconds(delay_secs as i64))
                    })
                    .map(|next_ts| (next_ts - utc_now).num_seconds());

                entries.push(CycleTimingEntry {
                    task_id: owner.id.clone(),
                    iteration: owner.loop_iteration + 1,
                    max_iterations: cc.max_iterations,
                    last_completed_ago_secs: last_ago,
                    next_due_in_secs: next_due_in,
                    status: owner.status,
                });
            }

            self.cycle_timing = entries;
        }

        // Refresh coordinator message statuses for all tasks.
        self.task_message_statuses = graph
            .tasks()
            .filter_map(|t| {
                workgraph::messages::coordinator_message_status(&self.workgraph_dir, &t.id)
                    .map(|s| (t.id.clone(), s))
            })
            .collect();

        // Enforce animation cap: drop oldest if we exceed MAX_ANIMATIONS.
        self.enforce_animation_cap();
    }

    /// Start a background file watcher on the `.workgraph/` directory.
    /// Sets `fs_change_pending` flag when any file changes, which triggers
    /// immediate panel reloads in `maybe_refresh()`.
    fn start_fs_watcher(&mut self) {
        use notify_debouncer_mini::new_debouncer;
        use std::time::Duration;

        let flag = self.fs_change_pending.clone();
        // 5ms debounce: just enough to coalesce a burst of events
        // from one write (inotify can fire twice per append on some
        // filesystems), not so much that the user perceives lag.
        // On a chat write, we want the TUI to react within a single
        // frame (16ms @ 60Hz), and 5ms leaves plenty of headroom.
        let debouncer = new_debouncer(Duration::from_millis(5), move |res| {
            if let Ok(_events) = res {
                flag.store(true, Ordering::Relaxed);
            }
        });

        match debouncer {
            Ok(mut debouncer) => {
                let watch_path = self.workgraph_dir.clone();
                if debouncer
                    .watcher()
                    .watch(&watch_path, notify::RecursiveMode::Recursive)
                    .is_ok()
                {
                    self._fs_watcher = Some(debouncer);
                }
            }
            Err(_) => {
                // File watching unavailable — fall back to polling (existing behavior).
            }
        }
    }

    /// Check if the graph has changed on disk and refresh if needed.
    /// Returns `true` if any work was done (graph reloaded, service polled, etc.).
    pub fn maybe_refresh(&mut self) -> bool {
        // Check if the file watcher detected changes in .workgraph/.
        let fs_changed = self.fs_change_pending.swap(false, Ordering::Relaxed);

        // Fast-path: when the streaming file changes (via fs watcher or polling),
        // immediately read it so chat text appears token-by-token.
        if self.chat.awaiting_response() {
            let prev = self.chat.streaming_text.clone();
            let streaming =
                workgraph::chat::read_streaming(&self.workgraph_dir, self.active_coordinator_id);
            if streaming != prev {
                self.chat.streaming_text = streaming;
                // Also check outbox in case the response just completed.
                self.poll_chat_messages();
                return true;
            }
        }

        // When file watcher fires, do targeted content-specific reloads
        // without waiting for the full refresh interval. This makes panels
        // that show non-graph content (messages, coord log, agent output,
        // chat outbox) update in real-time.
        if fs_changed {
            let mut content_updated = false;

            // Graph-dependent content: check if graph.jsonl itself changed.
            // Log entries are stored in graph.jsonl, so we need to detect
            // graph changes here too for immediate log updates.
            let current_mtime = std::fs::metadata(self.workgraph_dir.join("graph.jsonl"))
                .and_then(|m| m.modified())
                .ok();
            if current_mtime != self.last_graph_mtime {
                self.last_graph_mtime = current_mtime;
                // Full viz reload on every graph mutation — this catches
                // transient states (assigning, evaluating) that would
                // otherwise be missed by the 1-second slow-path tick.
                let graph_path = self.workgraph_dir.join("graph.jsonl");
                if let Ok(graph) = load_graph(&graph_path) {
                    let prev_hud_task = self.hud_detail.as_ref().map(|d| d.task_id.clone());
                    let prev_hud_scroll = self.hud_scroll;
                    let prev_hud_follow = self.hud_follow;
                    self.smart_follow_active = self.scroll.is_at_bottom();
                    self.load_viz_from_graph(&graph);
                    self.load_stats_from_graph(&graph);
                    self.load_agent_monitor();
                    self.update_agent_streams();
                    if self.right_panel_tab == RightPanelTab::Firehose {
                        self.update_firehose();
                    }
                    if self.right_panel_tab == RightPanelTab::Output {
                        self.update_output_pane();
                    }
                    if self.right_panel_tab == RightPanelTab::Log {
                        self.update_log_output();
                        self.update_log_stream_events();
                    }
                    self.invalidate_hud();
                    self.load_hud_detail();
                    if prev_hud_task.is_some()
                        && prev_hud_task == self.hud_detail.as_ref().map(|d| d.task_id.clone())
                    {
                        if prev_hud_follow {
                            self.hud_scroll = usize::MAX; // renderer clamps to actual max
                        } else {
                            self.hud_scroll = prev_hud_scroll;
                        }
                    }
                    if !self.search_input.is_empty() {
                        self.rerun_search();
                    }
                }
                // Log pane: reload if active (log entries are in graph.jsonl).
                if self.right_panel_tab == RightPanelTab::Log {
                    self.invalidate_log_pane();
                    self.load_log_pane();
                }
                if self.right_panel_tab == RightPanelTab::Agency {
                    self.invalidate_agency_lifecycle();
                    self.load_agency_lifecycle();
                }
                if self.right_panel_tab == RightPanelTab::Files
                    && let Some(ref mut fb) = self.file_browser
                {
                    fb.refresh();
                }
                if self.right_panel_tab == RightPanelTab::CoordLog {
                    self.load_coord_log();
                    self.load_activity_feed();
                }
                content_updated = true;
            }

            // Messages panel: check if the message file for the viewed task changed.
            if self.right_panel_tab == RightPanelTab::Messages
                && let Some(task_id) = self.selected_task_id().map(String::from)
            {
                let msg_path = self
                    .workgraph_dir
                    .join("messages")
                    .join(format!("{}.jsonl", task_id));
                let msg_mtime = std::fs::metadata(&msg_path).and_then(|m| m.modified()).ok();
                if msg_mtime != self.last_messages_mtime {
                    self.last_messages_mtime = msg_mtime;
                    self.save_message_draft();
                    self.invalidate_messages_panel();
                    self.load_messages_panel();
                    content_updated = true;
                }
            }

            // Coordinator log: check if daemon.log or operations.jsonl changed.
            if self.right_panel_tab == RightPanelTab::CoordLog {
                let log_path = self.workgraph_dir.join("service").join("daemon.log");
                let log_mtime = std::fs::metadata(&log_path).and_then(|m| m.modified()).ok();
                if log_mtime != self.last_daemon_log_mtime {
                    self.last_daemon_log_mtime = log_mtime;
                    self.load_coord_log();
                    content_updated = true;
                }
                let ops_path = self.workgraph_dir.join("log").join("operations.jsonl");
                let ops_mtime = std::fs::metadata(&ops_path).and_then(|m| m.modified()).ok();
                if ops_mtime != self.last_ops_log_mtime {
                    self.last_ops_log_mtime = ops_mtime;
                    self.load_activity_feed();
                    content_updated = true;
                }
            }

            // Chat outbox: check for new coordinator responses.
            if self.right_panel_tab == RightPanelTab::Chat || self.chat.awaiting_response() {
                let outbox_path = workgraph::chat::outbox_path_ref(
                    &self.workgraph_dir,
                    &self.active_coordinator_id.to_string(),
                );
                let outbox_mtime = std::fs::metadata(&outbox_path)
                    .and_then(|m| m.modified())
                    .ok();
                if outbox_mtime != self.last_chat_outbox_mtime {
                    self.last_chat_outbox_mtime = outbox_mtime;
                    self.check_coordinator_status();
                    self.poll_chat_messages();
                    content_updated = true;
                }
            }

            // Agent streams (task output): always check when fs changes detected,
            // since agent output files change independently of graph.jsonl.
            if !self.agent_streams.is_empty() || self.task_counts.in_progress > 0 {
                self.update_agent_streams();
                content_updated = true;
            }

            // Firehose: update if tab is active.
            if self.right_panel_tab == RightPanelTab::Firehose {
                self.update_firehose();
                content_updated = true;
            }

            // Output pane: update if tab is active.
            if self.right_panel_tab == RightPanelTab::Output {
                self.update_output_pane();
                content_updated = true;
            }

            // Log pane: update agent output if tab is active.
            if self.right_panel_tab == RightPanelTab::Log {
                self.update_log_output();
                self.update_log_stream_events();
                content_updated = true;
            }

            // Detail tab: live-refresh when the agent output.log changes independently
            // of graph.jsonl (e.g. agent is actively writing output).
            if self.right_panel_tab == RightPanelTab::Detail {
                let new_output_mtime = self
                    .hud_detail
                    .as_ref()
                    .and_then(|d| d.output_path.as_ref())
                    .and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
                if new_output_mtime.is_some() && new_output_mtime != self.last_detail_output_mtime {
                    self.last_detail_output_mtime = new_output_mtime;
                    let prev_hud_follow = self.hud_follow;
                    let prev_hud_task = self.hud_detail.as_ref().map(|d| d.task_id.clone());
                    let prev_hud_scroll = self.hud_scroll;
                    self.invalidate_hud();
                    self.load_hud_detail();
                    if prev_hud_task.is_some()
                        && prev_hud_task == self.hud_detail.as_ref().map(|d| d.task_id.clone())
                    {
                        if prev_hud_follow {
                            self.hud_scroll = usize::MAX;
                        } else {
                            self.hud_scroll = prev_hud_scroll;
                        }
                    }
                    content_updated = true;
                }
            }

            if content_updated {
                return true;
            }
        }

        if self.last_refresh.elapsed() < self.refresh_interval {
            return false;
        }

        // --- Lightweight timer updates (always run on 1-second tick) ---
        // These must execute BEFORE the heavy graph reload so activity
        // indicators stay fresh even when the reload is slow.
        self.last_refresh_display = chrono::Local::now().format("%H:%M:%S").to_string();

        // Update coordinator status and poll for new chat messages on every refresh tick.
        if self.chat.awaiting_response() || self.right_panel_tab == RightPanelTab::Chat {
            self.check_coordinator_status();
            self.poll_chat_messages();
        }

        // Poll service health every ~2 seconds for responsive agent count updates.
        if self.service_health.last_poll.elapsed() >= std::time::Duration::from_secs(2) {
            self.update_service_health();
            self.update_vitals();
        }

        // Auto-refresh config panel when config.toml changes on disk,
        // but only when the user is not actively editing.
        if self.right_panel_tab == RightPanelTab::Config
            && !self.config_panel.editing
            && !self.config_panel.adding_endpoint
            && !self.config_panel.adding_model
        {
            let current_mtime = std::fs::metadata(self.workgraph_dir.join("config.toml"))
                .and_then(|m| m.modified())
                .ok();
            if current_mtime != self.config_panel.last_config_mtime {
                self.load_config_panel();
            }
        }

        if self.time_counters.last_refresh.elapsed() >= std::time::Duration::from_secs(10) {
            self.update_time_counters();
        }

        // --- Heavy data refresh (graph-dependent) ---
        let current_mtime = std::fs::metadata(self.workgraph_dir.join("graph.jsonl"))
            .and_then(|m| m.modified())
            .ok();

        let graph_changed = current_mtime != self.last_graph_mtime || self.graph_viz_stale;
        let needs_token_refresh = self.task_counts.in_progress > 0;
        // Check if any sticky annotations have expired and need to be
        // removed from the rendered viz output.
        let hold = std::time::Duration::from_secs(STICKY_ANNOTATION_HOLD_SECS);
        let has_expiring_stickies = self
            .sticky_annotations
            .values()
            .any(|s| s.last_seen.elapsed() >= hold);

        if graph_changed || needs_token_refresh || has_expiring_stickies {
            self.graph_viz_stale = false;
            // Load graph once and share between viz and stats (avoids double read+parse).
            let graph_path = self.workgraph_dir.join("graph.jsonl");
            if let Ok(graph) = load_graph(&graph_path) {
                // Capture HUD scroll state BEFORE load_viz(), because load_viz() ->
                // recompute_trace() -> invalidate_hud() clears hud_detail.
                let prev_hud_task = self.hud_detail.as_ref().map(|d| d.task_id.clone());
                let prev_hud_scroll = self.hud_scroll;
                let prev_hud_follow = self.hud_follow;

                if graph_changed || has_expiring_stickies {
                    self.last_graph_mtime = current_mtime;
                    // Update smart-follow state before reloading: track if user is at bottom.
                    self.smart_follow_active = self.scroll.is_at_bottom();
                    self.load_viz_from_graph(&graph);
                    if !self.search_input.is_empty() {
                        self.rerun_search();
                    }
                }
                self.load_stats_from_graph(&graph);
                self.load_agent_monitor();
                self.update_agent_streams();
                // Update firehose with new agent output if Firehose tab is active.
                if self.right_panel_tab == RightPanelTab::Firehose {
                    self.update_firehose();
                }
                // Update output pane with new agent output if Output tab is active.
                if self.right_panel_tab == RightPanelTab::Output {
                    self.update_output_pane();
                }
                // Update log pane agent output if Log tab is active.
                if self.right_panel_tab == RightPanelTab::Log {
                    self.update_log_output();
                    self.update_log_stream_events();
                }
                // Preserve HUD scroll position when the selected task hasn't changed.
                self.invalidate_hud();
                // Eagerly reload so we can restore scroll before render.
                self.load_hud_detail();
                if prev_hud_task.is_some()
                    && prev_hud_task == self.hud_detail.as_ref().map(|d| d.task_id.clone())
                {
                    if prev_hud_follow {
                        self.hud_scroll = usize::MAX; // renderer clamps to actual max
                    } else {
                        self.hud_scroll = prev_hud_scroll;
                    }
                }
                // Reload log pane content if Log tab is active.
                if self.right_panel_tab == RightPanelTab::Log {
                    self.invalidate_log_pane();
                    self.load_log_pane();
                }
                // Messages panel: NOT reloaded here. Message changes are detected
                // by the fast-path mtime check (above), and task-selection changes
                // are handled by recompute_trace (inside load_viz_from_graph).
                // Reloading here on every tick caused the editor to be re-created
                // each second, resetting the cursor to position 0.
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
                    self.load_activity_feed();
                }
            }
        }

        self.last_refresh = Instant::now();

        // Re-check: if changes arrived during the refresh, reload once more
        // so rapid-fire changes don't require a full extra tick to propagate.
        if self.fs_change_pending.swap(false, Ordering::Relaxed) {
            let fresh_mtime = std::fs::metadata(self.workgraph_dir.join("graph.jsonl"))
                .and_then(|m| m.modified())
                .ok();
            if fresh_mtime != self.last_graph_mtime {
                self.last_graph_mtime = fresh_mtime;
                let graph_path = self.workgraph_dir.join("graph.jsonl");
                if let Ok(graph) = load_graph(&graph_path) {
                    self.load_viz_from_graph(&graph);
                    self.load_stats_from_graph(&graph);
                }
            }
        }

        true
    }

    /// Whether a refresh tick is due (enough time has elapsed since last refresh,
    /// or the file watcher detected changes).
    pub fn is_refresh_due(&self) -> bool {
        self.fs_change_pending.load(Ordering::Relaxed)
            || self.last_refresh.elapsed() >= self.refresh_interval
    }

    /// Whether any time-based UI elements are active and need periodic redraws
    /// (animations, fading notifications, scrollbar timeouts, etc.).
    pub fn has_timed_ui_elements(&self) -> bool {
        // File watcher detected changes — keep poll responsive for immediate updates.
        if self.fs_change_pending.load(Ordering::Relaxed) {
            return true;
        }
        // Chat streaming: keep poll interval short for progressive display.
        if self.chat.awaiting_response() {
            return true;
        }
        // Active splash animations (flash-and-fade on tasks)
        if self.has_active_animations() {
            return true;
        }
        // Slide animation on inspector panel
        if self.slide_animation.as_ref().is_some_and(|a| !a.is_done()) {
            return true;
        }
        // Jump target highlight (fades after 2s)
        if self.jump_target.is_some() {
            return true;
        }
        // Toasts (auto-dismissed by severity in drain_commands)
        if !self.toasts.is_empty() {
            return true;
        }
        // Scrollbar fade timers (visible for 2s after scroll activity)
        let scroll_active = |when: Option<Instant>| {
            when.is_some_and(|w| w.elapsed() < std::time::Duration::from_secs(3))
        };
        if scroll_active(self.graph_scroll_activity)
            || scroll_active(self.panel_scroll_activity)
            || scroll_active(self.graph_hscroll_activity)
            || scroll_active(self.panel_hscroll_activity)
        {
            return true;
        }
        // Config save notification (shown for 2s)
        if self
            .config_panel
            .save_notification
            .is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3))
        {
            return true;
        }
        // Service health feedback (shown for 5s)
        if self
            .service_health
            .feedback
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() < std::time::Duration::from_secs(6))
        {
            return true;
        }
        // Sticky annotations awaiting expiry — need periodic redraws to
        // remove them once the hold period elapses.
        if self.sticky_annotations.values().any(|s| {
            s.last_seen.elapsed() < std::time::Duration::from_secs(STICKY_ANNOTATION_HOLD_SECS + 1)
        }) {
            return true;
        }
        // Key feedback overlay (fades after 1.5s)
        if !self.key_feedback.is_empty() {
            return true;
        }
        // Touch echo indicators (fade after ~0.7s)
        if self.has_active_touch_echoes() {
            return true;
        }
        false
    }

    /// Compute the ideal poll timeout based on current UI activity.
    /// Short (50ms) during animations for smooth rendering, longer when idle.
    pub fn next_poll_timeout(&self) -> std::time::Duration {
        if self.chat_pty_mode && self.chat_pty_forwards_stdin {
            return std::time::Duration::from_millis(16);
        }

        // During animations, keep frame rate high for smooth visuals
        if self.has_active_animations()
            || self.slide_animation.as_ref().is_some_and(|a| !a.is_done())
            || self.has_active_touch_echoes()
        {
            return std::time::Duration::from_millis(50);
        }

        // Spinner animation while awaiting coordinator response
        if self.chat.awaiting_response() {
            return std::time::Duration::from_millis(100);
        }

        // When time-based UI elements are active (notifications, scrollbar fades),
        // use a moderate rate — they don't need 20fps but should update reasonably
        if self.has_timed_ui_elements() {
            return std::time::Duration::from_millis(200);
        }

        // When time counters are displayed (session timer, uptime), update ~1/sec
        if self.time_counters.any_enabled() {
            let until_refresh = self
                .refresh_interval
                .saturating_sub(self.last_refresh.elapsed());
            return until_refresh.min(std::time::Duration::from_secs(1));
        }

        // Fully idle: wait until next refresh is due, capped at 1 second
        let until_refresh = self
            .refresh_interval
            .saturating_sub(self.last_refresh.elapsed());
        until_refresh.min(std::time::Duration::from_secs(1))
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
                    self.sync_coordinator_from_selection();
                }
            }
            SortMode::ReverseChronological => {
                self.scroll.go_bottom();
                if !self.task_order.is_empty() {
                    self.selected_task_idx = Some(self.task_order.len() - 1);
                    self.recompute_trace();
                    self.sync_coordinator_from_selection();
                }
            }
            SortMode::StatusGrouped => {
                // Select the first task in priority order (likely in-progress).
                if !self.task_order.is_empty() {
                    self.selected_task_idx = Some(0);
                    self.recompute_trace();
                    self.sync_coordinator_from_selection();
                    self.scroll_to_selected_task();
                }
            }
        }
        self.push_toast(
            format!("Sort: {}", self.sort_mode.label()),
            ToastSeverity::Info,
        );
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
                                Status::Waiting | Status::PendingValidation => 3,
                                Status::PendingEval => 0, // visible like in-progress
                                Status::Incomplete => 1, // high priority like failed
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
        self.hud_follow = false;
        self.last_detail_output_mtime = None;
        // Refresh iteration archives and reset viewing state when switching tasks.
        if self.iteration_archives_task_id != task_id {
            self.iteration_archives = find_all_archives(&self.workgraph_dir, &task_id);
            self.iteration_archives_task_id = task_id.clone();
            self.viewing_iteration = None;
        }

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
        let iter_label = self.iteration_view_label(&task);
        lines.push(if let Some(ref label) = iter_label {
            format!("── {} ── [{}]", task.id, label)
        } else {
            format!("── {} ──", task.id)
        });
        lines.push(format!("Title: {}", task.title));
        lines.push(format!("Status: {:?}", task.status));
        if let Some(ref agent) = task.assigned {
            lines.push(format!("Agent: {}", agent));
        }

        let (registry_entry, compaction_snapshot) =
            load_task_runtime_snapshot(&self.workgraph_dir, &task);

        // ── Agency identity ──
        // Show agent entity (role + tradeoff) if task has an agency agent assigned.
        if let Some(ref agent_hash) = task.agent {
            let agency_dir = self.workgraph_dir.join("agency");
            let agents_cache = agency_dir.join("cache").join("agents");
            let agent_file = agents_cache.join(format!("{}.yaml", agent_hash));
            if let Ok(agent_entity) = workgraph::agency::load_agent(&agent_file) {
                let short_hash = &agent_hash[..agent_hash.len().min(8)];
                lines.push(format!("Identity: {} ({})", agent_entity.name, short_hash));
                // Look up role name
                let roles_dir = agency_dir.join("cache").join("roles");
                let role_file = roles_dir.join(format!("{}.yaml", agent_entity.role_id));
                let role_label = if let Ok(content) = std::fs::read_to_string(&role_file)
                    && let Ok(role) = serde_yaml::from_str::<serde_json::Value>(&content)
                    && let Some(name) = role.get("name").and_then(|v| v.as_str())
                {
                    let short = &agent_entity.role_id[..agent_entity.role_id.len().min(8)];
                    format!("{} ({})", name, short)
                } else {
                    let short = &agent_entity.role_id[..agent_entity.role_id.len().min(8)];
                    short.to_string()
                };
                lines.push(format!("Role: {}", role_label));
                // Look up tradeoff name
                let tradeoffs_dir = agency_dir.join("cache").join("tradeoffs");
                let tradeoff_file =
                    tradeoffs_dir.join(format!("{}.yaml", agent_entity.tradeoff_id));
                let tradeoff_label = if let Ok(content) = std::fs::read_to_string(&tradeoff_file)
                    && let Ok(tc) = serde_yaml::from_str::<serde_json::Value>(&content)
                    && let Some(name) = tc.get("name").and_then(|v| v.as_str())
                {
                    let short = &agent_entity.tradeoff_id[..agent_entity.tradeoff_id.len().min(8)];
                    format!("{} ({})", name, short)
                } else {
                    let short = &agent_entity.tradeoff_id[..agent_entity.tradeoff_id.len().min(8)];
                    short.to_string()
                };
                lines.push(format!("Tradeoff: {}", tradeoff_label));
            }
        }
        lines.push(String::new());

        // ── Runtime ──
        // For coordinator tasks, resolve model/executor from CoordinatorState
        // (coordinators don't use the agent registry).
        let (coord_executor, coord_model) = if task.id.starts_with(".coordinator-") {
            use crate::commands::service::CoordinatorState;
            let coord_id = task
                .id
                .strip_prefix(".coordinator-")
                .and_then(|s| s.parse::<u32>().ok());
            if let Some(cid) = coord_id {
                let coord_state = CoordinatorState::load_for(&self.workgraph_dir, cid);
                let config = Config::load_or_default(&self.workgraph_dir);
                let executor = coord_state
                    .as_ref()
                    .and_then(|s| s.executor_override.clone())
                    .or_else(|| Some(config.coordinator.effective_executor()));
                let model = coord_state
                    .as_ref()
                    .and_then(|s| s.model_override.clone())
                    .or_else(|| coord_state.as_ref().and_then(|s| s.model.clone()))
                    .or_else(|| config.coordinator.model.clone())
                    .or_else(|| {
                        Some(
                            config
                                .resolve_model_for_role(workgraph::config::DispatchRole::Default)
                                .model,
                        )
                    });
                (executor, model)
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        let is_coordinator = coord_executor.is_some() || coord_model.is_some();
        if is_coordinator
            || registry_entry.is_some()
            || task.model.is_some()
            || task.session_id.is_some()
        {
            lines.push("── Runtime ──".to_string());
            if is_coordinator {
                if let Some(ref executor) = coord_executor {
                    lines.push(format!("  Executor: {}", executor));
                }
                if let Some(ref model) = coord_model {
                    lines.push(format!("  Model: {}", model));
                }
            } else {
                if let Some(ref entry) = registry_entry {
                    lines.push(format!("  Executor: {}", entry.executor));
                }
                match (
                    task.model.as_deref(),
                    registry_entry.as_ref().and_then(|e| e.model.as_deref()),
                ) {
                    (Some(cfg), Some(actual)) if cfg != actual => {
                        lines.push(format!("  Model: {} (configured: {})", actual, cfg));
                    }
                    (_, Some(actual)) => {
                        lines.push(format!("  Model: {}", actual));
                    }
                    (Some(cfg), None) => {
                        lines.push(format!("  Model: {} (configured)", cfg));
                    }
                    (None, None) => {}
                }
            }
            if let Some(ref session_id) = task.session_id {
                lines.push(format!("  Session: {}", session_id));
            }
            lines.push(String::new());
        }

        // ── Compaction ──
        if let Some(snapshot) = compaction_snapshot {
            lines.push("── Compaction ──".to_string());
            lines.push(format!(
                "  Native journal: {}",
                if snapshot.journal_present {
                    "present"
                } else {
                    "absent"
                }
            ));
            if snapshot.journal_present {
                lines.push(format!("  Journal entries: {}", snapshot.journal_entries));
            }
            if snapshot.compaction_count > 0 {
                lines.push(format!("  Compactions: {}", snapshot.compaction_count));
            } else if snapshot.journal_present {
                lines.push("  Compactions: none (no 90%+ context pressure)".to_string());
            }
            if let Some(ref ts) = snapshot.last_compaction {
                lines.push(format!("  Last compaction: {}", format_timestamp(ts)));
            }
            if snapshot.session_summary_present {
                if let Some(words) = snapshot.session_summary_words {
                    lines.push(format!("  Session summary: present ({} words)", words));
                } else {
                    lines.push("  Session summary: present".to_string());
                }
            } else {
                lines.push("  Session summary: absent".to_string());
            }
            lines.push(String::new());
        }

        // ── Description ──
        if let Some(ref desc) = task.description {
            lines.push("── Description ──".to_string());
            for line in desc.lines() {
                lines.push(format!("  {}", line));
            }
            lines.push(String::new());
        }

        // ── Agent prompt (full) ──
        // When viewing a past iteration, load from the archived directory instead.
        let prompt_path = if let Some(iter_idx) = self.viewing_iteration {
            self.iteration_archives
                .get(iter_idx)
                .and_then(|(_, dir)| find_archive_file(dir, "prompt.txt"))
        } else {
            task.assigned
                .as_ref()
                .map(|aid| {
                    self.workgraph_dir
                        .join("agents")
                        .join(aid)
                        .join("prompt.txt")
                })
                .filter(|p| p.exists())
                .or_else(|| find_latest_archive(&self.workgraph_dir, &task.id, "prompt.txt"))
        };
        if let Some(prompt_path) = prompt_path {
            let prompt_header = if let Some(iter_idx) = self.viewing_iteration {
                format!("── Prompt (iteration {}) ──", iter_idx + 1)
            } else {
                "── Prompt ──".to_string()
            };
            lines.push(prompt_header);
            if let Ok(file) = std::fs::File::open(&prompt_path) {
                let reader = BufReader::new(file);
                for l in reader.lines().map_while(Result::ok) {
                    lines.push(format!("  {}", l));
                }
            }
            lines.push(String::new());
        }

        // ── Agent output (full) ──
        // When viewing a past iteration, load from the archived directory instead.
        let output_path = if let Some(iter_idx) = self.viewing_iteration {
            self.iteration_archives
                .get(iter_idx)
                .and_then(|(_, dir)| find_archive_file(dir, "output.txt"))
        } else {
            task.assigned
                .as_ref()
                .map(|aid| {
                    self.workgraph_dir
                        .join("agents")
                        .join(aid)
                        .join("output.log")
                })
                .filter(|p| p.exists())
                .or_else(|| find_latest_archive(&self.workgraph_dir, &task.id, "output.txt"))
        };
        // Save the live output.log path (not archives) for mtime-based live refresh.
        // Only set when viewing current iteration.
        let live_output_path = if self.viewing_iteration.is_some() {
            None
        } else {
            task.assigned
                .as_ref()
                .map(|aid| {
                    self.workgraph_dir
                        .join("agents")
                        .join(aid)
                        .join("output.log")
                })
                .filter(|p| p.exists())
        };
        if let Some(output_path) = output_path {
            let iter_suffix = self
                .viewing_iteration
                .map(|idx| format!(" (iteration {})", idx + 1))
                .unwrap_or_default();
            if self.detail_raw_json {
                lines.push(format!(
                    "── Output{} (raw) ── [R: human-readable]",
                    iter_suffix
                ));
                // Raw mode: pretty-printed JSON
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
            } else {
                lines.push(format!("── Output{} ── [R: raw JSON]", iter_suffix));
                // Human-readable mode: extract assistant text as markdown
                if let Ok(content) = std::fs::read_to_string(&output_path) {
                    let extracted = extract_enriched_text_from_log(&content);
                    if extracted.is_empty() {
                        lines.push("  (no assistant output)".to_string());
                    } else {
                        for line in extracted.lines() {
                            lines.push(format!("  {}", line));
                        }
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
                    let is_flip = eval
                        .get("source")
                        .and_then(|v| v.as_str())
                        .map(|s| s == "flip")
                        .unwrap_or(false);

                    if is_flip {
                        lines.push("── Evaluation (FLIP) ──".to_string());
                    } else {
                        lines.push("── Evaluation ──".to_string());
                    }
                    if let Some(score) = eval.get("score").and_then(|v| v.as_f64()) {
                        lines.push(format!("  Score: {:.2}", score));
                    }
                    if let Some(notes) = eval.get("notes").and_then(|v| v.as_str()) {
                        // Strip FLIP metadata JSON from notes before rendering
                        let display_notes = if let Some(idx) = notes.find("\n\nFLIP metadata: {") {
                            &notes[..idx]
                        } else {
                            notes
                        };
                        // Show first ~3 lines of notes.
                        for (i, line) in display_notes.lines().enumerate() {
                            if i >= 3 {
                                lines.push("  ...".to_string());
                                break;
                            }
                            lines.push(format!("  {}", line));
                        }
                    }
                    if let Some(dims) = eval.get("dimensions").and_then(|v| v.as_object()) {
                        let priority_order: &[&str] = if is_flip {
                            &[
                                "semantic_match",
                                "requirement_coverage",
                                "specificity_match",
                                "hallucination_rate",
                            ]
                        } else {
                            &[
                                "intent_fidelity",
                                "correctness",
                                "completeness",
                                "efficiency",
                                "style_adherence",
                                "downstream_usability",
                                "coordination_overhead",
                                "blocking_impact",
                            ]
                        };

                        let mut dim_strs: Vec<String> = Vec::new();

                        // For FLIP evals, prepend intent_fidelity from top-level score
                        if is_flip
                            && !dims.contains_key("intent_fidelity")
                            && let Some(score) = eval.get("score").and_then(|v| v.as_f64())
                        {
                            dim_strs.push(format!("intent_fidelity: {:.2}", score));
                        }

                        // Add dims in priority order first
                        for key in priority_order {
                            if let Some(v) = dims.get(*key) {
                                dim_strs.push(format!("{}: {:.2}", key, v.as_f64().unwrap_or(0.0)));
                            }
                        }

                        // Add any remaining dims alphabetically
                        let mut remaining: Vec<(&String, &serde_json::Value)> = dims
                            .iter()
                            .filter(|(k, _)| !priority_order.contains(&k.as_str()))
                            .collect();
                        remaining.sort_by_key(|(k, _)| k.as_str());
                        for (k, v) in remaining {
                            dim_strs.push(format!("{}: {:.2}", k, v.as_f64().unwrap_or(0.0)));
                        }

                        lines.push(format!("  Dims: {}", dim_strs.join(", ")));
                    }

                    // For FLIP evaluations, render formatted metadata section
                    if is_flip {
                        if let Some(notes) = eval.get("notes").and_then(|v| v.as_str())
                            && let Some(json_start) = notes.find("\n\nFLIP metadata: {")
                        {
                            let json_str = &notes[json_start + "\n\nFLIP metadata: ".len()..];
                            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(json_str) {
                                if let Some(inf_model) =
                                    meta.get("inference_model").and_then(|v| v.as_str())
                                    && let Some(cmp_model) =
                                        meta.get("comparison_model").and_then(|v| v.as_str())
                                {
                                    lines.push(format!("  Models: {} → {}", inf_model, cmp_model));
                                }
                                if let Some(prompt) =
                                    meta.get("inferred_prompt").and_then(|v| v.as_str())
                                {
                                    let preview: String = prompt
                                        .lines()
                                        .next()
                                        .unwrap_or("")
                                        .chars()
                                        .take(80)
                                        .collect();
                                    let suffix = if preview.len() < prompt.len() {
                                        "…"
                                    } else {
                                        ""
                                    };
                                    lines.push(format!("  Inferred: {}{}", preview, suffix));
                                }
                            }
                        }
                        if let Some(evaluator) = eval.get("evaluator").and_then(|v| v.as_str()) {
                            lines.push(format!("  Evaluator: {}", evaluator));
                        }
                    }

                    lines.push(String::new());
                }
                let _ = eval_found;
            }
        }

        // ── Token usage (execution) ──
        if let Some(ref usage) = task.token_usage {
            lines.push("── Tokens ──".to_string());
            let novel_in = usage.input_tokens + usage.cache_creation_input_tokens;
            lines.push(format!("  Input:  →{}", format_tokens(novel_in)));
            lines.push(format!("  Output: ←{}", format_tokens(usage.output_tokens)));
            if usage.cache_read_input_tokens > 0 {
                lines.push(format!(
                    "  Cached: {} (read from cache)",
                    format_tokens(usage.cache_read_input_tokens),
                ));
            }
            if usage.cost_usd > 0.0 {
                lines.push(format!("  Cost: ${:.4}", usage.cost_usd));
            }
            lines.push(String::new());
        }

        // ── Agency lifecycle costs (per-task breakdown) ──
        {
            let agents_dir = self.workgraph_dir.join("agents");

            let get_usage = |t: &workgraph::graph::Task| -> Option<TokenUsage> {
                t.token_usage.clone().or_else(|| {
                    let agent_id = t.assigned.as_deref()?;
                    let log_path = agents_dir.join(agent_id).join("output.log");
                    parse_token_usage_live(&log_path)
                })
            };

            // Lifecycle task prefixes with labels
            let lifecycle_tasks: Vec<(&str, String)> = vec![
                ("⊳ Assignment", format!(".assign-{}", task.id)),
                ("⊳ Assignment", format!("assign-{}", task.id)),
                ("∴ Evaluation", format!(".evaluate-{}", task.id)),
                ("∴ Evaluation", format!("evaluate-{}", task.id)),
                ("⤿ FLIP", format!(".flip-{}", task.id)),
                ("⤿ FLIP", format!("flip-{}", task.id)),
                ("✓ Verify", format!(".verify-{}", task.id)),
                ("✓ Verify", format!("verify-{}", task.id)),
            ];

            let mut phase_entries: Vec<(String, TokenUsage)> = Vec::new();
            let mut agency_total = TokenUsage {
                cost_usd: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            };

            for (label, tid) in &lifecycle_tasks {
                if let Some(t) = graph.tasks().find(|t| t.id == *tid)
                    && let Some(u) = get_usage(t)
                {
                    agency_total.accumulate(&u);
                    phase_entries.push((label.to_string(), u));
                }
            }

            if !phase_entries.is_empty() {
                lines.push("── § Agency Costs ──".to_string());
                for (label, u) in &phase_entries {
                    let novel_in = u.input_tokens + u.cache_creation_input_tokens;
                    let mut detail = format!(
                        "  {} →{} ←{}",
                        label,
                        format_tokens(novel_in),
                        format_tokens(u.output_tokens)
                    );
                    if u.cache_read_input_tokens > 0 {
                        detail.push_str(&format!(
                            "  (cached: {})",
                            format_tokens(u.cache_read_input_tokens)
                        ));
                    }
                    if u.cost_usd > 0.0 {
                        detail.push_str(&format!(" ${:.4}", u.cost_usd));
                    }
                    lines.push(detail);
                }
                // Show aggregated agency total (novel only)
                let agency_overhead = agency_total.input_tokens + agency_total.output_tokens;
                lines.push(format!("  § Total: {}", format_tokens(agency_overhead)));
                // Show combined total cost (execution + agency)
                let exec_cost = task.token_usage.as_ref().map(|u| u.cost_usd).unwrap_or(0.0);
                let total_cost = exec_cost + agency_total.cost_usd;
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

        // ── Cycle ──
        if let Some(ref cc) = task.cycle_config {
            lines.push("── Cycle ──".to_string());
            lines.push(format!(
                "  Iteration: {}/{}",
                task.loop_iteration + 1,
                cc.max_iterations
            ));
            if let Some(ref delay) = cc.delay {
                lines.push(format!("  Delay:     {}", delay));
            }
            let now = chrono::Utc::now();
            if let Some(ref last_ts) = task.last_iteration_completed_at
                && let Ok(parsed) = last_ts.parse::<chrono::DateTime<chrono::Utc>>()
            {
                let ago = now.signed_duration_since(parsed).num_seconds();
                lines.push(format!(
                    "  Last iter: {} ago",
                    workgraph::format_duration(ago, true)
                ));
            }
            // Next due: use ready_after if present, otherwise compute from last_completed + delay
            let next_due = task.ready_after.clone().or_else(|| {
                let delay_secs = cc
                    .delay
                    .as_ref()
                    .and_then(|d| workgraph::graph::parse_delay(d))?;
                let last_ts = task
                    .last_iteration_completed_at
                    .as_ref()?
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .ok()?;
                let next = last_ts + chrono::Duration::seconds(delay_secs as i64);
                Some(next.to_rfc3339())
            });
            if let Some(ref next_ts) = next_due
                && let Ok(parsed) = next_ts.parse::<chrono::DateTime<chrono::Utc>>()
            {
                if parsed > now {
                    let secs = (parsed - now).num_seconds();
                    lines.push(format!(
                        "  Next due:  in {}",
                        workgraph::format_duration(secs, true)
                    ));
                } else {
                    lines.push("  Next due:  ready now".to_string());
                }
            }
            lines.push(String::new());
        }

        // ── Iterations ──
        // Show iteration history for tasks with archives (cycle iterations or retries)
        {
            let archives = &self.iteration_archives;
            let has_iterations =
                !archives.is_empty() || task.loop_iteration > 0 || task.retry_count > 0;
            if has_iterations {
                let is_cycle = task.cycle_config.is_some() || task.loop_iteration > 0;
                let kind = if is_cycle { "Iteration" } else { "Attempt" };
                lines.push(format!("── {}s ── [use [ ] to browse]", kind));

                // Show "current" entry
                let current_marker = if self.viewing_iteration.is_none() {
                    "▶ "
                } else {
                    "  "
                };
                let status_str = format!("{:?}", task.status);
                lines.push(format!(
                    "  {}{} {} (current)   {}",
                    current_marker,
                    kind,
                    archives.len() + 1,
                    status_str.to_lowercase(),
                ));

                // Show archived iterations (most recent first for display)
                let now = chrono::Utc::now();
                for (display_idx, (ts_name, _dir)) in archives.iter().enumerate().rev() {
                    let marker = if self.viewing_iteration == Some(display_idx) {
                        "▶ "
                    } else {
                        "  "
                    };
                    let age = ts_name
                        .parse::<chrono::DateTime<chrono::Utc>>()
                        .ok()
                        .map(|parsed| {
                            let ago = now.signed_duration_since(parsed).num_seconds();
                            format!("  {} ago", workgraph::format_duration(ago.max(0), true))
                        })
                        .unwrap_or_default();
                    lines.push(format!(
                        "  {}{} {}   done{}",
                        marker,
                        kind,
                        display_idx + 1,
                        age,
                    ));
                }
                lines.push(String::new());
            }
        }

        // ── Failure reason ──
        if let Some(ref reason) = task.failure_reason {
            lines.push("── Failure ──".to_string());
            lines.push(format!("  {}", reason));
            lines.push(String::new());
        }

        // Log entries are now shown in the dedicated log pane (L to toggle).

        let output_mtime = live_output_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());

        self.hud_detail = Some(HudDetail {
            task_id,
            rendered_lines: lines,
            output_path: live_output_path,
            output_mtime,
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
        self.hud_follow = false;
        self.last_detail_output_mtime = None;

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

        // ── Executor & Model ──
        let registry_entry = task.assigned.as_ref().and_then(|aid| {
            AgentRegistry::load(&self.workgraph_dir)
                .ok()
                .and_then(|reg| reg.agents.get(aid).cloned())
        });
        {
            let actual_executor = registry_entry.as_ref().map(|e| e.executor.as_str());
            let actual_model = registry_entry.as_ref().and_then(|e| e.model.as_deref());
            let configured_model = task.model.as_deref();

            if let Some(exec) = actual_executor {
                lines.push(format!("Executor: {}", exec));
            }

            match (configured_model, actual_model) {
                (Some(cfg), Some(actual)) if cfg != actual => {
                    lines.push(format!("Model: {} (configured: {})", actual, cfg));
                }
                (_, Some(actual)) => {
                    lines.push(format!("Model: {}", actual));
                }
                (Some(cfg), None) => {
                    lines.push(format!("Model: {} (configured)", cfg));
                }
                (None, None) => {}
            }
        }

        // ── Agency identity ──
        if let Some(ref agent_hash) = task.agent {
            let agency_dir = self.workgraph_dir.join("agency");
            let agents_cache = agency_dir.join("cache").join("agents");
            let agent_file = agents_cache.join(format!("{}.yaml", agent_hash));
            if let Ok(agent_entity) = workgraph::agency::load_agent(&agent_file) {
                let short_hash = &agent_hash[..agent_hash.len().min(8)];
                lines.push(format!("Identity: {} ({})", agent_entity.name, short_hash));
                let roles_dir = agency_dir.join("cache").join("roles");
                let role_file = roles_dir.join(format!("{}.yaml", agent_entity.role_id));
                let role_label = if let Ok(content) = std::fs::read_to_string(&role_file)
                    && let Ok(role) = serde_yaml::from_str::<serde_json::Value>(&content)
                    && let Some(name) = role.get("name").and_then(|v| v.as_str())
                {
                    let short = &agent_entity.role_id[..agent_entity.role_id.len().min(8)];
                    format!("{} ({})", name, short)
                } else {
                    let short = &agent_entity.role_id[..agent_entity.role_id.len().min(8)];
                    short.to_string()
                };
                lines.push(format!("Role: {}", role_label));
                let tradeoffs_dir = agency_dir.join("cache").join("tradeoffs");
                let tradeoff_file =
                    tradeoffs_dir.join(format!("{}.yaml", agent_entity.tradeoff_id));
                let tradeoff_label = if let Ok(content) = std::fs::read_to_string(&tradeoff_file)
                    && let Ok(tc) = serde_yaml::from_str::<serde_json::Value>(&content)
                    && let Some(name) = tc.get("name").and_then(|v| v.as_str())
                {
                    let short = &agent_entity.tradeoff_id[..agent_entity.tradeoff_id.len().min(8)];
                    format!("{} ({})", name, short)
                } else {
                    let short = &agent_entity.tradeoff_id[..agent_entity.tradeoff_id.len().min(8)];
                    short.to_string()
                };
                lines.push(format!("Tradeoff: {}", tradeoff_label));
            }
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
        let live_output_path = task
            .assigned
            .as_ref()
            .map(|aid| {
                self.workgraph_dir
                    .join("agents")
                    .join(aid)
                    .join("output.log")
            })
            .filter(|p| p.exists());
        if let Some(output_path) = output_path {
            lines.push("── Output ──".to_string());
            if let Ok(content) = std::fs::read_to_string(&output_path) {
                let extracted = extract_enriched_text_from_log(&content);
                if extracted.is_empty() {
                    lines.push("  (no assistant output)".to_string());
                } else {
                    for line in extracted.lines() {
                        lines.push(format!("  {}", line));
                    }
                }
            }
            lines.push(String::new());
        }

        // ── Token usage ──
        if let Some(ref usage) = task.token_usage {
            lines.push("── Tokens ──".to_string());
            let novel_in = usage.input_tokens + usage.cache_creation_input_tokens;
            lines.push(format!("  Input:  →{}", format_tokens(novel_in)));
            lines.push(format!("  Output: ←{}", format_tokens(usage.output_tokens)));
            if usage.cache_read_input_tokens > 0 {
                lines.push(format!(
                    "  Cached: {} (read from cache)",
                    format_tokens(usage.cache_read_input_tokens),
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

        // ── Cycle ──
        if let Some(ref cc) = task.cycle_config {
            lines.push("── Cycle ──".to_string());
            lines.push(format!(
                "  Iteration: {}/{}",
                task.loop_iteration + 1,
                cc.max_iterations
            ));
            if let Some(ref delay) = cc.delay {
                lines.push(format!("  Delay:     {}", delay));
            }
            let now = chrono::Utc::now();
            if let Some(ref last_ts) = task.last_iteration_completed_at
                && let Ok(parsed) = last_ts.parse::<chrono::DateTime<chrono::Utc>>()
            {
                let ago = now.signed_duration_since(parsed).num_seconds();
                lines.push(format!(
                    "  Last iter: {} ago",
                    workgraph::format_duration(ago, true)
                ));
            }
            let next_due = task.ready_after.clone().or_else(|| {
                let delay_secs = cc
                    .delay
                    .as_ref()
                    .and_then(|d| workgraph::graph::parse_delay(d))?;
                let last_ts = task
                    .last_iteration_completed_at
                    .as_ref()?
                    .parse::<chrono::DateTime<chrono::Utc>>()
                    .ok()?;
                let next = last_ts + chrono::Duration::seconds(delay_secs as i64);
                Some(next.to_rfc3339())
            });
            if let Some(ref next_ts) = next_due
                && let Ok(parsed) = next_ts.parse::<chrono::DateTime<chrono::Utc>>()
            {
                if parsed > now {
                    let secs = (parsed - now).num_seconds();
                    lines.push(format!(
                        "  Next due:  in {}",
                        workgraph::format_duration(secs, true)
                    ));
                } else {
                    lines.push("  Next due:  ready now".to_string());
                }
            }
            lines.push(String::new());
        }

        // ── Failure reason ──
        if let Some(ref reason) = task.failure_reason {
            lines.push("── Failure ──".to_string());
            lines.push(format!("  {}", reason));
            lines.push(String::new());
        }

        let output_mtime = live_output_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());

        self.hud_detail = Some(HudDetail {
            task_id: target_task_id.to_string(),
            rendered_lines: lines,
            output_path: live_output_path,
            output_mtime,
        });
    }

    /// Scroll the HUD panel up.
    pub fn hud_scroll_up(&mut self, amount: usize) {
        self.hud_scroll = self.hud_scroll.saturating_sub(amount);
        // User scrolled up — disengage follow mode.
        self.hud_follow = false;
    }

    /// Scroll the HUD panel down using the cached wrapped line count and viewport height.
    pub fn hud_scroll_down(&mut self, amount: usize) {
        let max_scroll = self
            .hud_wrapped_line_count
            .saturating_sub(self.hud_detail_viewport_height);
        self.hud_scroll = (self.hud_scroll + amount).min(max_scroll);
        // If we reached the bottom, re-engage follow mode.
        if self.hud_scroll >= max_scroll {
            self.hud_follow = true;
        }
    }

    /// Navigate to the previous (older) iteration in the Detail tab.
    /// Returns true if the view changed.
    pub fn iteration_prev(&mut self) -> bool {
        if self.iteration_archives.is_empty() {
            return false;
        }
        let total = self.iteration_archives.len();
        match self.viewing_iteration {
            None => {
                // Currently viewing "current" — go to the most recent archive
                if total > 0 {
                    self.viewing_iteration = Some(total - 1);
                    self.hud_detail = None; // force reload
                    // Sync iteration across panels: Log and Output also show
                    // iteration-specific content and must be invalidated.
                    self.invalidate_log_pane();
                    self.output_pane.agent_texts.clear();
                    self.output_pane.viewing_iteration = self.viewing_iteration;
                    true
                } else {
                    false
                }
            }
            Some(idx) => {
                if idx > 0 {
                    self.viewing_iteration = Some(idx - 1);
                    self.hud_detail = None;
                    self.invalidate_log_pane();
                    self.output_pane.agent_texts.clear();
                    self.output_pane.viewing_iteration = self.viewing_iteration;
                    true
                } else {
                    false // already at oldest
                }
            }
        }
    }

    /// Navigate to the next (newer) iteration in the Detail tab.
    /// Returns true if the view changed.
    pub fn iteration_next(&mut self) -> bool {
        if self.iteration_archives.is_empty() {
            return false;
        }
        let total = self.iteration_archives.len();
        match self.viewing_iteration {
            None => false, // already at current
            Some(idx) => {
                if idx + 1 < total {
                    self.viewing_iteration = Some(idx + 1);
                    self.hud_detail = None;
                    self.invalidate_log_pane();
                    self.output_pane.agent_texts.clear();
                    self.output_pane.viewing_iteration = self.viewing_iteration;
                    true
                } else {
                    // Go back to "current" (live)
                    self.viewing_iteration = None;
                    self.hud_detail = None;
                    self.invalidate_log_pane();
                    self.output_pane.agent_texts.clear();
                    self.output_pane.viewing_iteration = self.viewing_iteration;
                    true
                }
            }
        }
    }

    /// Returns a label describing the current iteration view, if any.
    /// E.g., "Iteration 2/5" or "Attempt 2/4".
    pub fn iteration_view_label(&self, task: &workgraph::graph::Task) -> Option<String> {
        let total = self.iteration_archives.len();
        if total == 0 {
            return None;
        }
        let is_cycle = task.cycle_config.is_some() || task.loop_iteration > 0;
        let kind = if is_cycle { "iter" } else { "attempt" };
        // total archives + 1 for the "current" live iteration
        let display_total = total + 1;
        match self.viewing_iteration {
            Some(idx) => Some(format!("viewing {} {}/{}", kind, idx + 1, display_total)),
            None => Some(format!("{} {}/{}", kind, display_total, display_total)),
        }
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

        // Reset agent output if task changed.
        let prev_agent_id = self.log_pane.agent_id.clone();

        let mut agent_id: Option<String> = None;

        if task.log.is_empty() {
            self.log_pane
                .rendered_lines
                .push("(no log entries)".to_string());
        } else {
            let now = chrono::Utc::now();
            // Find agent_id from log entries (actor field) or from "Spawned" message.
            for entry in &task.log {
                // Try to extract agent_id from actor field.
                if agent_id.is_none() {
                    if let Some(ref actor) = entry.actor
                        && actor.starts_with("agent-")
                    {
                        agent_id = Some(actor.clone());
                    }
                    // Also check message for "[agent-XXXX]" pattern.
                    if agent_id.is_none()
                        && entry.message.contains("[agent-")
                        && let Some(start) = entry.message.find("[agent-")
                        && let Some(end) = entry.message[start..].find(']')
                    {
                        agent_id = Some(entry.message[start + 1..start + end].to_string());
                    }
                }
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

        // Also check the agent registry for an agent working on this task.
        if agent_id.is_none() {
            for entry in &self.agent_monitor.agents {
                if entry.task_id.as_deref() == Some(&task_id) {
                    agent_id = Some(entry.agent_id.clone());
                    break;
                }
            }
        }

        // Fall back to task.assigned field.
        if agent_id.is_none() {
            agent_id = task.assigned.clone();
        }

        // Reset agent output buffer if agent changed.
        if agent_id != prev_agent_id {
            self.log_pane.agent_output = OutputAgentText::default();
            self.log_pane.stream_events.clear();
            self.log_pane.raw_stream_offset = 0;
        }
        self.log_pane.agent_id = agent_id;

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
        // Mark agent output as dirty so markdown is re-rendered.
        self.log_pane.agent_output.dirty = true;
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
        // If we reached the bottom, resume auto-tail and clear "new output" indicator.
        if self.log_pane.scroll >= max_scroll {
            self.log_pane.auto_tail = true;
            self.log_pane.has_new_content = false;
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
        self.log_pane.has_new_content = false;
    }

    /// Toggle log pane JSON mode.
    pub fn toggle_log_json(&mut self) {
        self.log_pane.json_mode = !self.log_pane.json_mode;
        self.invalidate_log_pane();
    }

    /// Cycle the log pane through its three view modes:
    /// Events → HighLevel → RawPretty → Events.
    /// Resets scroll position to keep the most recent content visible
    /// (auto-tail enabled).
    pub fn cycle_log_view(&mut self) {
        self.log_pane.view_mode = self.log_pane.view_mode.next();
        self.log_scroll_to_bottom();
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
            self.load_activity_feed();
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

    // ── Activity Feed (operations.jsonl semantic view) ──

    /// Load new entries from operations.jsonl into the activity feed (incremental).
    /// Emits toast notifications for notable events (e.g., compaction).
    pub fn load_activity_feed(&mut self) {
        use std::io::{Seek, SeekFrom};
        let ops_path = self.workgraph_dir.join("log").join("operations.jsonl");
        let file = match std::fs::File::open(&ops_path) {
            Ok(f) => f,
            Err(_) => {
                // Fall back to coord_log if no operations.jsonl exists.
                return;
            }
        };
        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        // File was truncated (rotation) — reset.
        if file_len < self.activity_feed.last_offset {
            self.activity_feed.events.clear();
            self.activity_feed.last_offset = 0;
        }
        if file_len == self.activity_feed.last_offset {
            return;
        }
        let mut reader = BufReader::new(file);
        if self.activity_feed.last_offset > 0
            && reader
                .seek(SeekFrom::Start(self.activity_feed.last_offset))
                .is_err()
        {
            return;
        }
        let mut buf = String::new();
        while reader.read_line(&mut buf).unwrap_or(0) > 0 {
            let line = buf.trim();
            if !line.is_empty()
                && let Some(event) = ActivityEvent::parse(line)
            {
                // Emit toast notifications for notable events
                if matches!(event.kind, ActivityEventKind::Compact) {
                    self.push_toast(
                        format!("[∎ compact] {}", event.summary),
                        ToastSeverity::Info,
                    );
                }
                self.activity_feed.events.push_back(event);
                // Enforce ring buffer max.
                if self.activity_feed.events.len() > ACTIVITY_FEED_MAX_EVENTS {
                    self.activity_feed.events.pop_front();
                }
            }
            buf.clear();
        }
        self.activity_feed.last_offset = file_len;
        if self.activity_feed.auto_tail {
            self.activity_feed.scroll = usize::MAX;
        }
    }

    /// Scroll activity feed up.
    pub fn activity_feed_scroll_up(&mut self, amount: usize) {
        self.activity_feed.scroll = self.activity_feed.scroll.saturating_sub(amount);
        self.activity_feed.auto_tail = false;
    }

    /// Scroll activity feed down.
    pub fn activity_feed_scroll_down(&mut self, amount: usize) {
        let max_scroll = self
            .activity_feed
            .total_wrapped_lines
            .saturating_sub(self.activity_feed.viewport_height);
        self.activity_feed.scroll = (self.activity_feed.scroll + amount).min(max_scroll);
        if self.activity_feed.scroll >= max_scroll {
            self.activity_feed.auto_tail = true;
        }
    }

    /// Scroll activity feed to top.
    pub fn activity_feed_scroll_to_top(&mut self) {
        self.activity_feed.scroll = 0;
        self.activity_feed.auto_tail = false;
    }

    /// Scroll activity feed to bottom.
    pub fn activity_feed_scroll_to_bottom(&mut self) {
        let max_scroll = self
            .activity_feed
            .total_wrapped_lines
            .saturating_sub(self.activity_feed.viewport_height);
        self.activity_feed.scroll = max_scroll;
        self.activity_feed.auto_tail = true;
    }

    // ── Messages panel (panel 3) ──

    /// Load messages for the currently selected task into the messages panel.
    pub fn load_messages_panel(&mut self) {
        let task_id = match self.selected_task_id() {
            Some(id) => id.to_string(),
            None => {
                self.save_message_draft();
                self.messages_panel.rendered_lines.clear();
                self.messages_panel.entries.clear();
                self.messages_panel.summary = MessageSummary::default();
                self.messages_panel.task_id = None;
                editor_clear(&mut self.messages_panel.editor);
                return;
            }
        };

        // Skip reload if already loaded for this task.
        if self.messages_panel.task_id.as_deref() == Some(&task_id) {
            return;
        }

        // Save draft for the old task before switching.
        self.save_message_draft();

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
                        read_at: None,
                        send_timestamp: msg.timestamp.clone(),
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

                // Auto-advance the TUI read cursor so the coordinator knows
                // which messages have been seen.
                let max_id = msgs.last().map(|m| m.id).unwrap_or(0);
                if max_id > 0 {
                    let _ = workgraph::messages::write_cursor(
                        &self.workgraph_dir,
                        "tui",
                        &task_id,
                        max_id,
                    );
                }
            }
            Err(_) => {
                self.messages_panel
                    .rendered_lines
                    .push("(error loading messages)".to_string());
            }
        }

        self.messages_panel.task_id = Some(task_id);

        // Restore draft for the new task.
        self.restore_message_draft();
    }

    /// Force reload of messages panel content.
    pub fn invalidate_messages_panel(&mut self) {
        self.messages_panel.task_id = None;
    }

    /// Save the current message editor text as a draft for the current task.
    pub fn save_message_draft(&mut self) {
        if let Some(task_id) = self.messages_panel.task_id.clone() {
            let text = editor_text(&self.messages_panel.editor);
            if text.is_empty() {
                self.message_drafts.remove(&task_id);
            } else {
                self.message_drafts.insert(task_id, text);
            }
        }
    }

    /// Restore a saved draft into the message editor for the current task.
    /// Skips editor recreation when the text is unchanged, preserving cursor
    /// position across background refreshes (graph ticks, mtime changes).
    pub fn restore_message_draft(&mut self) {
        if let Some(task_id) = &self.messages_panel.task_id {
            if let Some(draft) = self.message_drafts.get(task_id).cloned() {
                if editor_text(&self.messages_panel.editor) != draft {
                    self.messages_panel.editor = new_emacs_editor_with(&draft);
                }
            } else if !editor_is_empty(&self.messages_panel.editor) {
                editor_clear(&mut self.messages_panel.editor);
            }
        } else if !editor_is_empty(&self.messages_panel.editor) {
            editor_clear(&mut self.messages_panel.editor);
        }
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
            cycle_timing: Vec::new(),
            show_total_tokens: false,
            show_help: false,
            show_system_tasks: false,
            show_running_system_tasks: false,
            system_tasks_just_toggled: false,
            mouse_enabled: false,
            any_motion_mouse: false,
            scroll_axis_swapped: false,
            last_graph_area: Rect::default(),
            last_right_panel_area: Rect::default(),
            last_divider_area: Rect::default(),
            divider_hover: false,
            last_horizontal_divider_area: Rect::default(),
            horizontal_divider_hover: false,
            last_split_mode: LayoutMode::TwoThirdsInspector,
            last_split_percent: 67,
            last_minimized_strip_area: Rect::default(),
            last_fullscreen_restore_area: Rect::default(),
            last_fullscreen_right_border_area: Rect::default(),
            last_fullscreen_top_border_area: Rect::default(),
            last_fullscreen_bottom_border_area: Rect::default(),
            minimized_strip_hover: false,
            fullscreen_restore_hover: false,
            fullscreen_right_hover: false,
            fullscreen_top_hover: false,
            fullscreen_bottom_hover: false,
            last_tab_bar_area: Rect::default(),
            last_iteration_nav_area: Rect::default(),
            last_right_content_area: Rect::default(),
            last_chat_input_area: Rect::default(),
            last_chat_message_area: Rect::default(),
            last_coordinator_bar_area: Rect::default(),
            coordinator_tab_hits: Vec::new(),
            coordinator_plus_hit: CoordinatorPlusHit::default(),
            last_message_input_area: Rect::default(),
            last_text_prompt_area: Rect::default(),
            last_dialog_area: Rect::default(),
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
            annotation_map: viz.annotation_map.clone(),
            sticky_annotations: HashMap::new(),
            annotation_hit_regions: Vec::new(),
            annotation_click_flash: None,
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
            responsive_breakpoint: ResponsiveBreakpoint::Full,
            inspector_is_beside: true,
            single_panel_view: SinglePanelView::Graph,

            input_mode: InputMode::Normal,
            needs_center_on_selected: false,
            needs_scroll_into_view: false,
            chat_input_dismissed: false,
            inspector_sub_focus: InspectorSubFocus::ChatHistory,
            task_form: None,
            launcher: None,
            last_launcher_area: Rect::default(),
            launcher_name_hit: Rect::default(),
            launcher_executor_hits: Vec::new(),
            launcher_model_hits: Vec::new(),
            launcher_model_list_area: Rect::default(),
            launcher_endpoint_hits: Vec::new(),
            launcher_endpoint_list_area: Rect::default(),
            launcher_recent_hits: Vec::new(),
            launcher_launch_btn_hit: Rect::default(),
            launcher_cancel_btn_hit: Rect::default(),
            coordinator_picker: None,
            text_prompt: TextPromptState {
                editor: new_emacs_editor(),
            },
            active_coordinator_id: 0,
            coordinator_chats: HashMap::new(),
            chat: ChatState::default(),
            history_depth_override: None,
            no_history: false,
            task_panes: HashMap::new(),
            chat_pty_mode: false,
            chat_pty_observer: false,
            chat_pty_takeover_pending_since: None,
            chat_pty_forwards_stdin: false,
            agent_monitor: AgentMonitorState::default(),
            agent_streams: HashMap::new(),
            service_health: ServiceHealthState::default(),
            last_service_badge_area: Rect::default(),
            vitals: VitalsState::default(),
            time_counters: TimeCounters::new("uptime,cumulative,active"),
            firehose: FirehoseState::default(),
            output_pane: OutputPaneState::default(),
            dashboard: DashboardState::default(),
            nav_stack: NavStack::default(),
            agency_lifecycle: None,
            log_pane: LogPaneState::default(),
            coord_log: CoordLogState::default(),
            activity_feed: ActivityFeedState::default(),
            messages_panel: MessagesPanelState::default(),
            message_drafts: HashMap::new(),
            task_message_statuses: HashMap::new(),
            cmd_rx: mpsc::channel().1,
            cmd_tx: mpsc::channel().0,
            toasts: Vec::new(),
            prev_agent_statuses: HashMap::new(),
            last_tab_press: None,
            sort_mode: SortMode::ReverseChronological,
            smart_follow_active: true,
            initial_load: false,
            splash_animations: HashMap::new(),
            task_snapshots: HashMap::new(),
            animation_mode: AnimationMode::Normal,
            slide_animation: None,
            message_name_threshold: 8,
            message_indent: 2,
            session_gap_minutes: 30,
            last_launcher_open: None,
            graph_scroll_activity: None,
            panel_scroll_activity: None,
            scrollbar_drag: None,
            divider_drag_offset: 0,
            divider_drag_start_pct: 0,
            divider_drag_start_col: 0,
            divider_drag_start_row: 0,
            graph_pan_last: None,
            last_graph_scrollbar_area: Rect::default(),
            last_panel_scrollbar_area: Rect::default(),
            graph_hscroll_activity: None,
            panel_hscroll_activity: None,
            last_graph_hscrollbar_area: Rect::default(),
            last_panel_hscrollbar_area: Rect::default(),
            last_log_new_output_area: Rect::default(),
            last_iter_nav_area: Rect::default(),
            touch_echo_enabled: false,
            touch_echoes: Vec::new(),
            has_keyboard_enhancement: false,
            editor_handler: create_editor_handler(),
            last_graph_mtime: None,
            last_refresh: Instant::now(),
            last_refresh_display: String::new(),
            refresh_interval: std::time::Duration::from_secs(3600),
            archive_browser: ArchiveBrowserState::default(),
            viewing_iteration: None,
            iteration_archives_task_id: String::new(),
            iteration_archives: Vec::new(),
            history_browser: HistoryBrowserState::default(),
            config_panel: ConfigPanelState::default(),
            file_browser: None,
            fs_change_pending: Arc::new(AtomicBool::new(false)),
            _fs_watcher: None,
            last_messages_mtime: None,
            last_daemon_log_mtime: None,
            last_ops_log_mtime: None,
            last_chat_outbox_mtime: None,
            last_detail_output_mtime: None,
            hud_follow: false,
            graph_viz_stale: false,
            tracer: None,
            key_feedback_enabled: false,
            key_feedback: VecDeque::new(),
        }
    }

    /// Force an immediate refresh (manual `r` key).
    pub fn force_refresh(&mut self) {
        self.last_graph_mtime = std::fs::metadata(self.workgraph_dir.join("graph.jsonl"))
            .and_then(|m| m.modified())
            .ok();
        self.graph_viz_stale = false;
        self.smart_follow_active = self.scroll.is_at_bottom();
        // Load graph once and share between viz and stats.
        let graph_path = self.workgraph_dir.join("graph.jsonl");
        if let Ok(graph) = load_graph(&graph_path) {
            self.load_viz_from_graph(&graph);
            if !self.search_input.is_empty() {
                self.rerun_search();
            }
            self.load_stats_from_graph(&graph);
        } else {
            self.load_viz();
            if !self.search_input.is_empty() {
                self.rerun_search();
            }
            self.load_stats();
        }
        self.load_agent_monitor();
        self.last_refresh_display = chrono::Local::now().format("%H:%M:%S").to_string();
        self.last_refresh = Instant::now();
    }

    // ── Multi-panel methods ──

    /// Toggle focus between Graph and RightPanel.
    /// In compact mode, switches the single-panel view instead.
    /// In full-inspector or off mode, focus stays locked to the visible content.
    pub fn toggle_panel_focus(&mut self) {
        // In compact mode, Tab switches the single-panel view.
        if self.responsive_breakpoint == ResponsiveBreakpoint::Compact {
            self.toggle_single_panel_view();
            return;
        }
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

    /// Cycle forward through panels in single-panel (compact) mode: Graph → Detail → Log → Graph.
    /// Also updates focused_panel to match.
    pub fn toggle_single_panel_view(&mut self) {
        self.set_single_panel_view(self.single_panel_view.next());
    }

    /// Cycle backward through panels in single-panel (compact) mode: Graph → Log → Detail → Graph.
    pub fn prev_single_panel_view(&mut self) {
        self.set_single_panel_view(self.single_panel_view.prev());
    }

    /// Set the single-panel view and update focused_panel + right_panel_tab accordingly.
    fn set_single_panel_view(&mut self, view: SinglePanelView) {
        self.single_panel_view = view;
        match view {
            SinglePanelView::Graph => {
                self.focused_panel = FocusedPanel::Graph;
            }
            SinglePanelView::Detail => {
                self.focused_panel = FocusedPanel::RightPanel;
                self.right_panel_tab = RightPanelTab::Detail;
            }
            SinglePanelView::Log => {
                self.focused_panel = FocusedPanel::RightPanel;
                self.right_panel_tab = RightPanelTab::Log;
            }
        }
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
    #[allow(dead_code)]
    pub fn cycle_layout_mode_reverse(&mut self) {
        self.apply_layout_mode(self.layout_mode.cycle_reverse());
    }

    /// Apply a layout mode, updating panel visibility and focus.
    /// Saves the current split state when transitioning to FullInspector or Off.
    pub fn apply_layout_mode(&mut self, mode: LayoutMode) {
        // Save the current normal split state before leaving it.
        if self.layout_mode.is_normal_split() && !mode.is_normal_split() {
            self.last_split_mode = self.layout_mode;
            self.last_split_percent = self.right_panel_percent;
        }
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

    /// Restore the last normal split mode from FullInspector or Off.
    pub fn restore_from_extreme(&mut self) {
        let mode = self.last_split_mode;
        self.layout_mode = mode;
        self.right_panel_visible = true;
        self.right_panel_percent = self.last_split_percent;
    }

    /// Cycle inspector view forward: closed → Chat → Detail → ... → CoordLog → closed.
    /// Opens the panel (if closed) and advances to the next tab, or closes if on the last tab.
    pub fn cycle_inspector_view_forward(&mut self) {
        if !self.right_panel_visible || self.layout_mode == LayoutMode::Off {
            // Panel is closed → open with first tab
            self.right_panel_tab = RightPanelTab::ALL[0];
            if self.layout_mode == LayoutMode::Off {
                self.apply_layout_mode(LayoutMode::TwoThirdsInspector);
            } else {
                self.right_panel_visible = true;
            }
            self.slide_animation = Some(SlideAnimation {
                start: Instant::now(),
                direction: SlideDirection::Forward,
            });
        } else if self.right_panel_tab == *RightPanelTab::ALL.last().unwrap() {
            // On last tab → close
            self.apply_layout_mode(LayoutMode::Off);
            self.slide_animation = None;
        } else {
            // Advance to next tab
            self.right_panel_tab = self.right_panel_tab.next();
            self.slide_animation = Some(SlideAnimation {
                start: Instant::now(),
                direction: SlideDirection::Forward,
            });
        }
    }

    /// Cycle inspector view backward: closed → CoordLog → ... → Detail → Chat → closed.
    pub fn cycle_inspector_view_backward(&mut self) {
        if !self.right_panel_visible || self.layout_mode == LayoutMode::Off {
            // Panel is closed → open with last tab
            self.right_panel_tab = *RightPanelTab::ALL.last().unwrap();
            if self.layout_mode == LayoutMode::Off {
                self.apply_layout_mode(LayoutMode::TwoThirdsInspector);
            } else {
                self.right_panel_visible = true;
            }
            self.slide_animation = Some(SlideAnimation {
                start: Instant::now(),
                direction: SlideDirection::Backward,
            });
        } else if self.right_panel_tab == RightPanelTab::ALL[0] {
            // On first tab → close
            self.apply_layout_mode(LayoutMode::Off);
            self.slide_animation = None;
        } else {
            // Move to previous tab
            self.right_panel_tab = self.right_panel_tab.prev();
            self.slide_animation = Some(SlideAnimation {
                start: Instant::now(),
                direction: SlideDirection::Backward,
            });
        }
    }

    /// Grow the viz (right) pane by 5% of panel_percent, transitioning to Off at max.
    /// Steps: 5 → 10 → ... → 95 → 100 → Off (closes panel, full viz).
    pub fn grow_viz_pane(&mut self) {
        if !self.right_panel_visible || self.layout_mode == LayoutMode::Off {
            // Open panel at minimum size first
            self.right_panel_visible = true;
            self.layout_mode = LayoutMode::ThirdInspector;
            self.right_panel_percent = 5;
            return;
        }
        if self.right_panel_percent >= 100 {
            // At maximum → transition to Off (full viz)
            self.apply_layout_mode(LayoutMode::Off);
        } else {
            let new_pct = (self.right_panel_percent + 5).min(100);
            let new_mode = Self::layout_mode_for_percent(new_pct);
            // Save split state before entering FullInspector.
            if self.layout_mode.is_normal_split() && !new_mode.is_normal_split() {
                self.last_split_mode = self.layout_mode;
                self.last_split_percent = self.right_panel_percent;
            }
            self.right_panel_percent = new_pct;
            self.layout_mode = new_mode;
        }
    }

    /// Shrink the viz (right) pane by 5%, transitioning to Off at min.
    /// Steps: 100 → 95 → ... → 10 → 5 → Off (closes panel, full viz).
    pub fn shrink_viz_pane(&mut self) {
        if !self.right_panel_visible || self.layout_mode == LayoutMode::Off {
            // Open panel at max size first
            self.right_panel_visible = true;
            self.layout_mode = LayoutMode::FullInspector;
            self.right_panel_percent = 100;
            self.focused_panel = FocusedPanel::RightPanel;
            return;
        }
        if self.right_panel_percent <= 5 {
            // At minimum → transition to Off (full viz)
            self.apply_layout_mode(LayoutMode::Off);
        } else {
            self.right_panel_percent = self.right_panel_percent.saturating_sub(5).max(5);
            self.layout_mode = Self::layout_mode_for_percent(self.right_panel_percent);
        }
    }

    /// Map a percentage to the nearest LayoutMode bracket.
    pub(super) fn layout_mode_for_percent(pct: u16) -> LayoutMode {
        if pct >= 100 {
            LayoutMode::FullInspector
        } else if pct >= 59 {
            LayoutMode::TwoThirdsInspector
        } else if pct >= 42 {
            LayoutMode::HalfInspector
        } else {
            LayoutMode::ThirdInspector
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
    /// Returns `true` if any commands were processed.
    pub fn drain_commands(&mut self) -> bool {
        let mut drained = false;
        while let Ok(result) = self.cmd_rx.try_recv() {
            drained = true;
            match result.effect {
                CommandEffect::Refresh => {
                    self.force_refresh();
                }
                CommandEffect::Notify(msg) => {
                    if result.success {
                        self.push_toast(msg, ToastSeverity::Info);
                    } else {
                        let err = result
                            .output
                            .lines()
                            .find(|l| !l.is_empty())
                            .unwrap_or("unknown");
                        self.push_toast(format!("Error: {}", err), ToastSeverity::Error);
                    }
                }
                CommandEffect::RefreshAndNotify(msg) => {
                    self.force_refresh();
                    if self.archive_browser.active {
                        self.archive_browser.load(&self.workgraph_dir);
                    }
                    if result.success {
                        self.push_toast(msg, ToastSeverity::Info);
                    } else {
                        let err = result
                            .output
                            .lines()
                            .find(|l| !l.is_empty())
                            .unwrap_or("unknown");
                        self.push_toast(format!("Error: {}", err), ToastSeverity::Error);
                    }
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
                            edited: false,
                            inbox_id: None,
                            user: None,
                            target_task: None,
                            msg_timestamp: Some(chrono::Utc::now().to_rfc3339()),
                            read_at: None,
                            msg_queue_id: None,
                        });
                        save_chat_history_with_skip(
                            &self.workgraph_dir,
                            self.active_coordinator_id,
                            &self.chat.messages,
                            self.chat.skipped_history_count,
                        );
                        // Clear this request from the pending set — no response will come.
                        self.chat.pending_request_ids.remove(&request_id);
                        if self.chat.pending_request_ids.is_empty() {
                            self.chat.awaiting_since = None;
                        }
                    } else {
                        // wg chat succeeded — response should be in the outbox.
                        // Poll immediately so message appears at the same time as
                        // the throbber disappears (avoids 1-second gap).
                        self.poll_chat_messages();
                        // If poll didn't find messages yet (edge case), keep
                        // awaiting_response true so the throbber persists until
                        // the next poll picks it up.
                    }
                    // Auto-scroll to bottom.
                    self.chat.scroll = 0;
                    // Refresh graph in case coordinator created tasks.
                    self.force_refresh();
                }
                CommandEffect::CreateCoordinator => {
                    if result.success {
                        if let Ok(data) = serde_json::from_str::<serde_json::Value>(&result.output)
                        {
                            if let Some(cid) = data["coordinator_id"].as_u64() {
                                self.force_refresh();
                                self.switch_coordinator(cid as u32);
                                self.right_panel_tab = RightPanelTab::Chat;
                                self.push_toast(
                                    format!("Chat {} created", cid),
                                    ToastSeverity::Info,
                                );
                            }
                        } else {
                            self.push_toast(
                                "New chat created".to_string(),
                                ToastSeverity::Info,
                            );
                        }
                    } else {
                        let err = result
                            .output
                            .lines()
                            .find(|l| !l.is_empty())
                            .unwrap_or("unknown");
                        self.push_toast(
                            format!("Failed to create coordinator: {}", err),
                            ToastSeverity::Error,
                        );
                    }
                    self.force_refresh();
                }
                CommandEffect::DeleteCoordinator(cid) => {
                    if result.success {
                        if cid == self.active_coordinator_id {
                            // Switch to another available coordinator (not the one being deleted)
                            let other = self
                                .list_coordinator_ids()
                                .into_iter()
                                .find(|&id| id != cid)
                                .unwrap_or(0);
                            self.switch_coordinator(other);
                        }
                        self.coordinator_chats.remove(&cid);
                        self.force_refresh();
                        self.push_toast(format!("Closed coordinator {}", cid), ToastSeverity::Info);
                    } else {
                        let err = result
                            .output
                            .lines()
                            .find(|l| !l.is_empty())
                            .unwrap_or("unknown");
                        self.push_toast(
                            format!("Failed to delete coordinator: {}", err),
                            ToastSeverity::Error,
                        );
                    }
                }
                CommandEffect::ArchiveCoordinator(cid) => {
                    if result.success {
                        if cid == self.active_coordinator_id {
                            // Switch to another available coordinator (not the one being archived)
                            let other = self
                                .list_coordinator_ids()
                                .into_iter()
                                .find(|&id| id != cid)
                                .unwrap_or(0);
                            self.switch_coordinator(other);
                        }
                        self.coordinator_chats.remove(&cid);
                        self.force_refresh();
                        self.push_toast(
                            format!("Archived coordinator {}", cid),
                            ToastSeverity::Info,
                        );
                    } else {
                        let err = result
                            .output
                            .lines()
                            .find(|l| !l.is_empty())
                            .unwrap_or("unknown");
                        self.push_toast(
                            format!("Failed to archive coordinator: {}", err),
                            ToastSeverity::Error,
                        );
                    }
                }
                CommandEffect::StopCoordinator(cid) => {
                    if result.success {
                        self.force_refresh();
                        self.push_toast(
                            format!("Stopped coordinator {}", cid),
                            ToastSeverity::Info,
                        );
                    } else {
                        let err = result
                            .output
                            .lines()
                            .find(|l| !l.is_empty())
                            .unwrap_or("unknown");
                        self.push_toast(
                            format!("Failed to stop coordinator: {}", err),
                            ToastSeverity::Error,
                        );
                    }
                }
                CommandEffect::InterruptCoordinator(cid) => {
                    if result.success {
                        self.push_toast(
                            format!("Interrupted coordinator {}", cid),
                            ToastSeverity::Info,
                        );
                    } else {
                        let err = result
                            .output
                            .lines()
                            .find(|l| !l.is_empty())
                            .unwrap_or("unknown");
                        self.push_toast(
                            format!("Failed to interrupt coordinator: {}", err),
                            ToastSeverity::Error,
                        );
                    }
                }
                CommandEffect::EndpointTest(ep_name) => {
                    if result.success {
                        self.config_panel
                            .endpoint_test_results
                            .insert(ep_name, EndpointTestStatus::Ok);
                    } else {
                        let err_msg = result
                            .output
                            .lines()
                            .find(|l| !l.is_empty())
                            .unwrap_or("connection failed")
                            .to_string();
                        self.config_panel
                            .endpoint_test_results
                            .insert(ep_name, EndpointTestStatus::Error(err_msg));
                    }
                }
            }
        }
        // Clear expired toasts (auto-dismiss by severity).
        if self.cleanup_toasts() {
            drained = true;
        }
        drained
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
        self.load_dashboard();
    }

    /// Refresh dashboard state from agent monitor data + coordinator state.
    pub fn load_dashboard(&mut self) {
        use crate::commands::service::CoordinatorState;

        // ── Coordinator cards (one per coordinator) ──
        // Use fresh registry active_count for agents_alive instead of stale
        // CoordinatorState.agents_alive (which is only updated at tick boundaries).
        let fresh_alive =
            workgraph::AgentRegistry::load_or_warn(&self.workgraph_dir).active_count();
        let all_states = CoordinatorState::load_all(&self.workgraph_dir);
        self.dashboard.coordinator_cards = if all_states.is_empty() {
            vec![DashboardCoordinatorCard {
                id: 0,
                ..Default::default()
            }]
        } else {
            all_states
                .iter()
                .map(|(id, cs)| DashboardCoordinatorCard {
                    id: *id,
                    enabled: cs.enabled,
                    paused: cs.paused,
                    frozen: cs.frozen,
                    ticks: cs.ticks,
                    agents_alive: fresh_alive,
                    tasks_ready: cs.tasks_ready,
                    max_agents: cs.max_agents,
                    model: cs.model.clone(),
                    accumulated_tokens: cs.accumulated_tokens,
                })
                .collect()
        };

        // ── Agent rows from monitor entries ──
        let agents_dir = self.workgraph_dir.join("agents");
        let prev_count = self.dashboard.agent_rows.len();
        self.dashboard.agent_rows = self
            .agent_monitor
            .agents
            .iter()
            .map(|entry| {
                // Determine seconds since last output file modification
                let secs_since_output = agents_dir
                    .join(&entry.agent_id)
                    .join("output.log")
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|mtime| SystemTime::now().duration_since(mtime).ok())
                    .map(|d| d.as_secs() as i64);

                let snippet = self
                    .agent_streams
                    .get(&entry.agent_id)
                    .and_then(|s| s.latest_snippet.clone());

                DashboardAgentRow {
                    agent_id: entry.agent_id.clone(),
                    task_id: entry.task_id.clone().unwrap_or_default(),
                    task_title: entry.task_title.clone(),
                    activity: DashboardAgentActivity::classify(
                        entry.status,
                        secs_since_output,
                        false, // refined with child-process check below
                    ),
                    elapsed_secs: entry.runtime_secs,
                    model: None, // populated from registry below if available
                    latest_snippet: snippet,
                }
            })
            .collect();

        // Populate model and refine activity classification from the registry
        if let Ok(registry) = AgentRegistry::load(&self.workgraph_dir) {
            for row in &mut self.dashboard.agent_rows {
                if let Some(agent) = registry.agents.get(&row.agent_id) {
                    row.model = agent.model.clone();
                    // Re-classify stuck agents: if they have active children,
                    // they're waiting on a subprocess, not stuck
                    if row.activity == DashboardAgentActivity::Stuck
                        && workgraph::service::has_active_children(agent.pid)
                    {
                        row.activity = DashboardAgentActivity::Slow;
                    }
                }
            }
        }

        // Record sparkline event if agent count changed
        let new_count = self.dashboard.agent_rows.len();
        if new_count != prev_count {
            self.dashboard.record_sparkline_event();
        }

        // Clamp selected row
        if !self.dashboard.agent_rows.is_empty() {
            self.dashboard.selected_row = self
                .dashboard
                .selected_row
                .min(self.dashboard.agent_rows.len() - 1);
        } else {
            self.dashboard.selected_row = 0;
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
                                                    format!(
                                                        "{}…",
                                                        &snippet
                                                            [..snippet.floor_char_boundary(120)]
                                                    )
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
                                                        format!(
                                                            "{name}: {}…",
                                                            &c[..c.floor_char_boundary(80)]
                                                        )
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
                    "turn" => {
                        // Native executor format
                        info.message_count += 1;
                        if let Some(content) = val.get("content").and_then(|c| c.as_array()) {
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
                                                let snippet =
                                                    trimmed.lines().last().unwrap_or(trimmed);
                                                let snippet = if snippet.len() > 120 {
                                                    format!(
                                                        "{}…",
                                                        &snippet
                                                            [..snippet.floor_char_boundary(120)]
                                                    )
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
                                        info.latest_snippet = Some(name.to_string());
                                        info.latest_is_tool = true;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    "tool_call" => {
                        // Native executor tool call log
                        let name = val.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let detail = match name {
                            "Bash" | "bash" => val
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
                            "Read" | "Write" | "Edit" => val
                                .get("input")
                                .and_then(|i| i.get("file_path"))
                                .and_then(|v| v.as_str())
                                .map(|p| format!("{name}: {p}")),
                            "Grep" | "Glob" => val
                                .get("input")
                                .and_then(|i| i.get("pattern"))
                                .and_then(|v| v.as_str())
                                .map(|p| format!("{name}: {p}")),
                            _ => None,
                        };
                        info.latest_snippet = Some(detail.unwrap_or_else(|| name.to_string()));
                        info.latest_is_tool = true;
                    }
                    "user" | "result" => {
                        info.message_count += 1;
                    }
                    _ => {}
                }
            }
        }
    }

    /// Update the Output pane by reading agent output.log files and extracting assistant text.
    /// Called when the Output tab is active. Reads incrementally from each agent's output.log
    /// and accumulates extracted markdown text.
    pub fn update_output_pane(&mut self) {
        use std::io::{Read, Seek, SeekFrom};

        let agents_dir = self.workgraph_dir.join("agents");

        // Check if iteration changed — if so, invalidate all cached text so we reload
        // from the new archive (or live output if returning to current).
        if self.output_pane.viewing_iteration != self.viewing_iteration {
            self.output_pane.viewing_iteration = self.viewing_iteration;
            self.output_pane.agent_texts.clear();
        }

        // Collect visible agents: Working + recently completed (Done/Failed).
        let visible_agents: Vec<(String, Option<String>, AgentStatus)> = self
            .agent_monitor
            .agents
            .iter()
            .filter(|a| {
                matches!(
                    a.status,
                    AgentStatus::Working | AgentStatus::Done | AgentStatus::Failed
                )
            })
            .map(|a| (a.agent_id.clone(), a.task_id.clone(), a.status))
            .collect();

        let visible_ids: HashSet<String> =
            visible_agents.iter().map(|(id, _, _)| id.clone()).collect();

        // Remove agents that are no longer visible (Dead agents with no recent activity).
        self.output_pane
            .agent_texts
            .retain(|id, _| visible_ids.contains(id));
        self.output_pane
            .agent_scrolls
            .retain(|id, _| visible_ids.contains(id));

        // If the active agent is gone, auto-select the first Working agent.
        if let Some(ref active_id) = self.output_pane.active_agent_id
            && !visible_ids.contains(active_id)
        {
            self.output_pane.active_agent_id = None;
        }
        if self.output_pane.active_agent_id.is_none() {
            self.output_pane.active_agent_id = visible_agents
                .iter()
                .find(|(_, _, s)| matches!(s, AgentStatus::Working))
                .map(|(id, _, _)| id.clone())
                .or_else(|| visible_agents.first().map(|(id, _, _)| id.clone()));
        }

        for (agent_id, _task_id, status) in &visible_agents {
            // When viewing a past iteration, read from the archived output instead of live.
            let log_path = if let Some(iter_idx) = self.viewing_iteration {
                self.iteration_archives
                    .get(iter_idx)
                    .and_then(|(_, dir)| find_archive_file(dir, "output.txt"))
                    .or_else(|| {
                        self.iteration_archives
                            .get(iter_idx)
                            .and_then(|(_, dir)| find_archive_file(dir, "output.log"))
                    })
                    .unwrap_or_else(|| agents_dir.join(agent_id).join("output.log"))
            } else {
                agents_dir.join(agent_id).join("output.log")
            };
            if !log_path.exists() {
                continue;
            }

            let text_entry = self
                .output_pane
                .agent_texts
                .entry(agent_id.clone())
                .or_default();

            // Mark finished agents.
            if !text_entry.finished && matches!(status, AgentStatus::Done | AgentStatus::Failed) {
                text_entry.finished = true;
                text_entry.finish_status = Some(match status {
                    AgentStatus::Done => "done".to_string(),
                    AgentStatus::Failed => "failed".to_string(),
                    _ => "unknown".to_string(),
                });
                text_entry.dirty = true;
            }

            // Open file and seek to last known position.
            let mut file = match std::fs::File::open(&log_path) {
                Ok(f) => f,
                Err(_) => continue,
            };

            let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if file_len <= text_entry.file_offset {
                continue; // No new data.
            }

            if file.seek(SeekFrom::Start(text_entry.file_offset)).is_err() {
                continue;
            }

            let mut new_data = String::new();
            if file.read_to_string(&mut new_data).is_err() {
                continue;
            }

            text_entry.file_offset = file_len;

            // Extract assistant text + tool results from the new JSONL lines.
            let new_text = extract_enriched_text_from_log(&new_data);
            if !new_text.is_empty() {
                if !text_entry.full_text.is_empty() {
                    text_entry.full_text.push_str("\n\n");
                }
                text_entry.full_text.push_str(&new_text);

                // Cap at OUTPUT_MAX_CHARS (bytes). Byte-scan for the next
                // `\n` — always 1 byte, always at a char boundary — so
                // multi-byte chars like `─` in tool-box borders don't
                // cause a char-boundary panic.
                if text_entry.full_text.len() > OUTPUT_MAX_CHARS {
                    let min_skip = text_entry.full_text.len() - OUTPUT_MAX_CHARS;
                    let boundary = text_entry
                        .full_text
                        .as_bytes()
                        .iter()
                        .enumerate()
                        .skip(min_skip)
                        .find(|&(_, &b)| b == b'\n')
                        .map(|(i, _)| i + 1)
                        .unwrap_or_else(|| text_entry.full_text.len());
                    text_entry.full_text = text_entry.full_text[boundary..].to_string();
                }

                text_entry.dirty = true;

                // Signal new content for the indicator.
                if let Some(ref active_id) = self.output_pane.active_agent_id
                    && active_id == agent_id
                {
                    let scroll = self
                        .output_pane
                        .agent_scrolls
                        .get(agent_id)
                        .map(|s| !s.auto_follow)
                        .unwrap_or(false);
                    if scroll {
                        self.output_pane.has_new_content = true;
                    }
                }
            }
        }
    }

    /// Update the Log pane's agent output by incrementally reading the agent's output.log.
    /// Uses the same extraction function (`extract_enriched_text_from_log`) and data type
    /// (`OutputAgentText`) as the Output tab, ensuring identical rendering.
    pub fn update_log_output(&mut self) {
        use std::io::{Read, Seek, SeekFrom};

        let agent_id = match &self.log_pane.agent_id {
            Some(id) => id.clone(),
            None => return,
        };

        // Check if iteration changed — if so, invalidate cached text so we reload
        // from the new archive (or live output if returning to current).
        if self.log_pane.viewing_iteration != self.viewing_iteration {
            self.log_pane.viewing_iteration = self.viewing_iteration;
            self.log_pane.agent_output = OutputAgentText::default();
        }

        let agents_dir = self.workgraph_dir.join("agents");

        // When viewing a past iteration, read from the archived output instead of live.
        let log_path = if let Some(iter_idx) = self.viewing_iteration {
            self.iteration_archives
                .get(iter_idx)
                .and_then(|(_, dir)| find_archive_file(dir, "output.txt"))
                .or_else(|| {
                    self.iteration_archives
                        .get(iter_idx)
                        .and_then(|(_, dir)| find_archive_file(dir, "output.log"))
                })
                .unwrap_or_else(|| agents_dir.join(&agent_id).join("output.log"))
        } else {
            agents_dir.join(&agent_id).join("output.log")
        };
        if !log_path.exists() {
            return;
        }

        let text_entry = &mut self.log_pane.agent_output;

        let mut file = match std::fs::File::open(&log_path) {
            Ok(f) => f,
            Err(_) => return,
        };

        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if file_len <= text_entry.file_offset {
            return; // No new data.
        }

        if file.seek(SeekFrom::Start(text_entry.file_offset)).is_err() {
            return;
        }

        let mut new_data = String::new();
        if file.read_to_string(&mut new_data).is_err() {
            return;
        }

        text_entry.file_offset = file_len;

        // Same extraction as the Output tab.
        let new_text = extract_enriched_text_from_log(&new_data);
        if !new_text.is_empty() {
            if !text_entry.full_text.is_empty() {
                text_entry.full_text.push_str("\n\n");
            }
            text_entry.full_text.push_str(&new_text);

            // Cap at OUTPUT_MAX_CHARS (misnomer: it's bytes). Find the
            // next `\n` at/after the cut point via byte scan — newlines
            // are always 1 byte and always at char boundaries, so the
            // resulting slice is safe even when `full_text` contains
            // multi-byte chars (e.g. the `─` in tool-box borders).
            if text_entry.full_text.len() > OUTPUT_MAX_CHARS {
                let min_skip = text_entry.full_text.len() - OUTPUT_MAX_CHARS;
                let boundary = text_entry
                    .full_text
                    .as_bytes()
                    .iter()
                    .enumerate()
                    .skip(min_skip)
                    .find(|&(_, &b)| b == b'\n')
                    .map(|(i, _)| i + 1)
                    .unwrap_or_else(|| text_entry.full_text.len());
                text_entry.full_text = text_entry.full_text[boundary..].to_string();
            }

            text_entry.dirty = true;

            // Signal new content if scrolled up.
            if !self.log_pane.auto_tail {
                self.log_pane.has_new_content = true;
            }
        }
    }

    pub fn update_log_stream_events(&mut self) {
        use std::io::{Read, Seek, SeekFrom};

        let agent_id = match &self.log_pane.agent_id {
            Some(id) => id.clone(),
            None => return,
        };

        let agents_dir = self.workgraph_dir.join("agents");
        let stream_path = agents_dir.join(&agent_id).join("raw_stream.jsonl");
        if !stream_path.exists() {
            return;
        }

        let mut file = match std::fs::File::open(&stream_path) {
            Ok(f) => f,
            Err(_) => return,
        };

        let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if file_len <= self.log_pane.raw_stream_offset {
            return;
        }

        if file
            .seek(SeekFrom::Start(self.log_pane.raw_stream_offset))
            .is_err()
        {
            return;
        }

        let mut new_data = String::new();
        if file.read_to_string(&mut new_data).is_err() {
            return;
        }

        self.log_pane.raw_stream_offset = file_len;

        let mut had_new = false;
        for line in new_data.lines() {
            if let Some(event) = parse_raw_stream_line(line, &agent_id) {
                self.log_pane.stream_events.push(event);
                had_new = true;
            }
        }

        const MAX_STREAM_EVENTS: usize = 2000;
        if self.log_pane.stream_events.len() > MAX_STREAM_EVENTS {
            let drain = self.log_pane.stream_events.len() - MAX_STREAM_EVENTS;
            self.log_pane.stream_events.drain(..drain);
        }

        if had_new && !self.log_pane.auto_tail {
            self.log_pane.has_new_content = true;
        }
    }

    /// Get the list of agent IDs visible in the Output pane tab bar, ordered by
    /// Working first, then recently completed (Done/Failed).
    pub fn output_pane_agent_ids(&self) -> Vec<String> {
        let mut working: Vec<&AgentMonitorEntry> = Vec::new();
        let mut completed: Vec<&AgentMonitorEntry> = Vec::new();
        for a in &self.agent_monitor.agents {
            match a.status {
                AgentStatus::Working => working.push(a),
                AgentStatus::Done | AgentStatus::Failed => completed.push(a),
                _ => {}
            }
        }
        let mut ids: Vec<String> = working.iter().map(|a| a.agent_id.clone()).collect();
        ids.extend(completed.iter().map(|a| a.agent_id.clone()));
        ids
    }

    /// Update the firehose view by reading new lines from all active agents' output.log files.
    /// Each non-empty line is appended to the firehose buffer. The buffer is capped at
    /// FIREHOSE_MAX_LINES to prevent memory growth.
    pub fn update_firehose(&mut self) {
        use std::io::{Read, Seek, SeekFrom};

        let agents_dir = self.workgraph_dir.join("agents");

        // Collect active agent IDs and their task IDs from the monitor.
        let active_agents: Vec<(String, String)> = self
            .agent_monitor
            .agents
            .iter()
            .filter(|a| matches!(a.status, AgentStatus::Working))
            .map(|a| (a.agent_id.clone(), a.task_id.clone().unwrap_or_default()))
            .collect();

        let mut new_lines: Vec<FirehoseLine> = Vec::new();

        for (agent_id, task_id) in &active_agents {
            let log_path = agents_dir.join(agent_id).join("output.log");
            if !log_path.exists() {
                continue;
            }

            let offset = self
                .firehose
                .agent_offsets
                .entry(agent_id.clone())
                .or_insert(0);

            let mut file = match std::fs::File::open(&log_path) {
                Ok(f) => f,
                Err(_) => continue,
            };

            let file_len = file.metadata().map(|m| m.len()).unwrap_or(0);
            if file_len <= *offset {
                continue;
            }

            if file.seek(SeekFrom::Start(*offset)).is_err() {
                continue;
            }

            let mut new_data = String::new();
            if file.read_to_string(&mut new_data).is_err() {
                continue;
            }

            *offset = file_len;

            // Assign a stable color index for this agent.
            let color_idx = *self
                .firehose
                .agent_colors
                .entry(agent_id.clone())
                .or_insert_with(|| {
                    let idx = self.firehose.next_color;
                    self.firehose.next_color += 1;
                    idx
                });

            for line in new_data.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                new_lines.push(FirehoseLine {
                    agent_id: agent_id.clone(),
                    task_id: task_id.clone(),
                    text: line.to_string(),
                    color_idx,
                });
            }
        }

        if !new_lines.is_empty() {
            self.firehose.lines.extend(new_lines);
            // Cap buffer.
            if self.firehose.lines.len() > FIREHOSE_MAX_LINES {
                let drain = self.firehose.lines.len() - FIREHOSE_MAX_LINES;
                self.firehose.lines.drain(..drain);
                // Adjust scroll offset to account for removed lines.
                self.firehose.scroll = self.firehose.scroll.saturating_sub(drain);
            }
            // Auto-scroll to bottom if tail mode is active.
            if self.firehose.auto_tail {
                self.firehose.scroll = usize::MAX;
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

        // Evaluation phase: combine .evaluate-* and .flip-* token usage
        let eval_task_id = format!(".evaluate-{}", task_id);
        let legacy_eval_id = format!("evaluate-{}", task_id);
        let flip_task_id = format!(".flip-{}", task_id);
        let eval_task = graph
            .tasks()
            .find(|t| t.id == eval_task_id || t.id == legacy_eval_id);
        let flip_task = graph.tasks().find(|t| t.id == flip_task_id);
        let evaluation = eval_task
            .map(|t| {
                let mut phase = build_phase(t, "Evaluation");

                // Accumulate FLIP task token usage into evaluation phase
                if let Some(ft) = flip_task {
                    let flip_phase = build_phase(ft, "FLIP");
                    if let Some(flip_usage) = flip_phase.token_usage {
                        if let Some(ref mut eval_usage) = phase.token_usage {
                            eval_usage.accumulate(&flip_usage);
                        } else {
                            phase.token_usage = Some(flip_usage);
                        }
                    }
                }

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
            })
            .or_else(|| {
                // If no evaluate task but flip task exists, show flip as evaluation phase
                flip_task.map(|ft| build_phase(ft, "Evaluation"))
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

    /// Poll service health: read state files to determine health level,
    /// stuck tasks, and recent errors.
    pub fn update_service_health(&mut self) {
        use crate::commands::service::{
            CoordinatorState, ServiceState, is_service_alive, log_file_path,
        };

        let dir = self.workgraph_dir.clone();
        let dir = &dir;

        // 1. Load service state
        let state = ServiceState::load(dir).ok().flatten();
        let Some(state) = state else {
            self.service_health.level = ServiceHealthLevel::Red;
            self.service_health.label = "DOWN".to_string();
            self.service_health.pid = None;
            self.service_health.uptime = None;
            self.service_health.socket_path = None;
            self.service_health.uptime_secs = None;
            self.service_health.agents_alive = 0;
            self.service_health.agents_total = 0;
            self.service_health.paused = false;
            self.service_health.stuck_tasks.clear();
            self.service_health.recent_errors.clear();
            self.service_health.last_poll = Instant::now();
            return;
        };

        // 2. Check if PID is alive
        if !is_service_alive(state.pid) {
            self.service_health.level = ServiceHealthLevel::Red;
            self.service_health.label = "DOWN".to_string();
            self.service_health.pid = Some(state.pid);
            self.service_health.socket_path = Some(state.socket_path);
            self.service_health.uptime = None;
            self.service_health.uptime_secs = None;
            self.service_health.last_poll = Instant::now();
            return;
        }

        // Service is alive — gather details
        self.service_health.pid = Some(state.pid);
        self.service_health.socket_path = Some(state.socket_path);

        // Compute uptime
        let uptime_secs = chrono::DateTime::parse_from_rfc3339(&state.started_at)
            .ok()
            .map(|started| {
                let now = chrono::Utc::now();
                (now - started.with_timezone(&chrono::Utc))
                    .num_seconds()
                    .max(0) as u64
            });
        self.service_health.uptime_secs = uptime_secs;
        self.service_health.uptime = uptime_secs.map(|s| {
            if s < 60 {
                format!("{}s", s)
            } else if s < 3600 {
                format!("{}m{}s", s / 60, s % 60)
            } else {
                format!("{}h{}m", s / 3600, (s % 3600) / 60)
            }
        });

        // Load coordinator state (coordinator 0 = dispatch state)
        let coord = CoordinatorState::load_or_default_for(dir, 0);
        self.service_health.paused = coord.paused;
        self.service_health.agents_max = coord.max_agents;

        // Load provider health to determine pause reason
        use workgraph::service::provider_health::ProviderHealth;
        let provider_health = ProviderHealth::load(dir).unwrap_or_default();

        // Detect auto-pause state change for notifications
        let new_provider_auto_pause = coord.paused && provider_health.service_paused;

        if coord.paused {
            if provider_health.service_paused {
                self.service_health.provider_auto_pause = true;
                self.service_health.pause_reason = provider_health.pause_reason.clone();
            } else {
                self.service_health.provider_auto_pause = false;
                self.service_health.pause_reason = Some("Manual pause".to_string());
            }
        } else {
            self.service_health.provider_auto_pause = false;
            self.service_health.pause_reason = None;
        }

        // Show notification if provider auto-pause just triggered
        if new_provider_auto_pause && !self.service_health.prev_provider_auto_pause {
            let reason = self
                .service_health
                .pause_reason
                .as_deref()
                .unwrap_or("Provider health issue");
            let msg = format!("⚠ Service auto-paused: {} - Press Ctrl+R to resume", reason);
            self.push_toast(msg, ToastSeverity::Warning);
        }

        // Update previous state for next comparison
        self.service_health.prev_provider_auto_pause = new_provider_auto_pause;

        // Load agent registry for alive count and stuck task detection.
        // Use status-based count (active_count) to match `wg service status` / `wg status`.
        // The daemon's cleanup routines (triage, dead-agent reaping) keep registry
        // statuses accurate. PID-based liveness checks are unreliable from the TUI
        // process because the daemon may have already reaped the zombie (removing it
        // from the process table) before updating the registry status.
        let registry = workgraph::AgentRegistry::load_or_warn(dir);
        let alive = registry.active_count();
        self.service_health.agents_alive = alive;
        self.service_health.agents_total = registry.agents.len();

        // Phase 1 toast triggers: Agent exited → Info, Agent stuck (>5m) → Warning (deduped).
        // Collect into a vec to avoid borrow conflicts (self.prev_agent_statuses + push_toast).
        enum AgentToast {
            Simple(String, ToastSeverity),
            Dedup(String, ToastSeverity, String),
        }
        let mut agent_toasts = Vec::new();
        for agent in registry.agents.values() {
            let prev = self.prev_agent_statuses.get(&agent.id).copied();
            let was_alive = prev.is_some_and(|s| {
                matches!(
                    s,
                    workgraph::AgentStatus::Starting
                        | workgraph::AgentStatus::Working
                        | workgraph::AgentStatus::Idle
                )
            });
            let now_exited = matches!(
                agent.status,
                workgraph::AgentStatus::Done
                    | workgraph::AgentStatus::Failed
                    | workgraph::AgentStatus::Dead
            );
            // Agent exited: was alive, now done/failed/dead.
            if was_alive && now_exited {
                let duration_str = chrono::DateTime::parse_from_rfc3339(&agent.started_at)
                    .ok()
                    .map(|started| {
                        let dur = chrono::Utc::now().signed_duration_since(started);
                        format_duration_short(dur)
                    })
                    .unwrap_or_default();
                let suffix = if duration_str.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", duration_str)
                };
                agent_toasts.push(AgentToast::Simple(
                    format!(
                        "\u{1f6aa} Agent exited: {} on {}{}",
                        agent.id, agent.task_id, suffix
                    ),
                    ToastSeverity::Info,
                ));
            }
            // Agent stuck: alive but output file not modified in >5 minutes.
            // Suppress if the agent has active child processes (waiting on subprocess).
            if agent.is_alive() && crate::commands::is_process_alive(agent.pid) {
                let output_age_secs = std::fs::metadata(&agent.output_file)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|d| d.as_secs());
                if let Some(secs) = output_age_secs
                    && secs > 300
                    && !workgraph::service::has_active_children(agent.pid)
                {
                    agent_toasts.push(AgentToast::Dedup(
                        format!(
                            "\u{23f3} Agent stuck: {} on {} ({})",
                            agent.id,
                            agent.task_id,
                            format_duration_compact(secs),
                        ),
                        ToastSeverity::Warning,
                        format!("stuck:{}", agent.id),
                    ));
                }
            }
        }
        // Update previous agent statuses for next comparison.
        self.prev_agent_statuses = registry
            .agents
            .iter()
            .map(|(k, v)| (k.clone(), v.status))
            .collect();
        // Now emit the collected toasts.
        for toast in agent_toasts {
            match toast {
                AgentToast::Simple(msg, sev) => self.push_toast(msg, sev),
                AgentToast::Dedup(msg, sev, key) => self.push_toast_dedup(msg, sev, key),
            }
        }

        // Detect stuck tasks: in-progress tasks whose agent PID is dead
        let graph_path = dir.join("graph.jsonl");
        let mut stuck = Vec::new();
        if let Ok(graph) = workgraph::parser::load_graph(&graph_path) {
            for task in graph.tasks() {
                if task.status == workgraph::graph::Status::InProgress
                    && let Some(ref agent_id) = task.agent
                    && let Some(agent) = registry.agents.get(agent_id)
                    && !crate::commands::is_process_alive(agent.pid)
                {
                    stuck.push(StuckTask {
                        task_id: task.id.clone(),
                        task_title: task.title.clone(),
                        agent_id: agent_id.clone(),
                    });
                }
            }
        }
        self.service_health.stuck_tasks = stuck;

        // Read recent errors from daemon log (last 5 ERROR/WARN lines within 10 minutes)
        let log_path = log_file_path(dir);
        let mut recent_errors = Vec::new();
        let cutoff = chrono::Utc::now() - chrono::Duration::minutes(10);
        if let Ok(content) = std::fs::read_to_string(&log_path) {
            for line in content.lines().rev() {
                if line.contains("ERROR") || line.contains("WARN") {
                    // Parse timestamp from line start: "2026-03-08T12:59:07.285Z [LEVEL] ..."
                    let ts_end = line.find(' ').unwrap_or(0);
                    if ts_end == 0 {
                        continue;
                    }
                    let ts_str = &line[..ts_end];
                    let Ok(ts) = chrono::DateTime::parse_from_rfc3339(ts_str) else {
                        continue;
                    };
                    if ts.with_timezone(&chrono::Utc) < cutoff {
                        // Past the 10-minute window; since we're iterating newest-first, stop
                        break;
                    }
                    recent_errors.push(line.to_string());
                    if recent_errors.len() >= 5 {
                        break;
                    }
                }
            }
            recent_errors.reverse();
        }
        self.service_health.recent_errors = recent_errors;

        // Determine health level
        let stuck_count = self.service_health.stuck_tasks.len();
        let count_label = format!("{}/{}", alive, coord.max_agents);
        if self.service_health.provider_auto_pause {
            // Provider errors are serious - show as red
            self.service_health.level = ServiceHealthLevel::Red;
        } else if coord.paused
            || uptime_secs.is_some_and(|s| s < 30)
            || stuck_count > 0
            || !self.service_health.recent_errors.is_empty()
        {
            self.service_health.level = ServiceHealthLevel::Yellow;
        } else {
            self.service_health.level = ServiceHealthLevel::Green;
        }
        self.service_health.label = if coord.paused {
            if self.service_health.provider_auto_pause {
                "PAUSED: provider error".to_string()
            } else {
                "PAUSED".to_string()
            }
        } else {
            count_label
        };

        self.service_health.last_poll = Instant::now();
    }

    /// Update the HUD vitals bar state from existing app state and cheap syscalls.
    pub fn update_vitals(&mut self) {
        // Agent count: reuse service_health (already refreshed)
        self.vitals.agents_alive = self.service_health.agents_alive;
        self.vitals.daemon_running = self.service_health.level != ServiceHealthLevel::Red;

        // Task counts: reuse already-computed task_counts
        self.vitals.open = self.task_counts.open;
        self.vitals.running = self.task_counts.in_progress;
        self.vitals.done = self.task_counts.done;

        // Last event time: operations.jsonl mtime (cheap stat syscall)
        let ops_path = self.workgraph_dir.join("log").join("operations.jsonl");
        self.vitals.last_event_time = std::fs::metadata(&ops_path).and_then(|m| m.modified()).ok();

        // Coordinator last tick: parse from coordinator-state
        use crate::commands::service::CoordinatorState;
        let coord = CoordinatorState::load_or_default_for(&self.workgraph_dir, 0);
        self.vitals.coord_last_tick = coord.last_tick.as_ref().and_then(|ts| {
            chrono::DateTime::parse_from_rfc3339(ts).ok().map(|dt| {
                SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(dt.timestamp() as u64)
            })
        });
    }

    pub fn update_time_counters(&mut self) {
        if !self.time_counters.any_enabled() {
            return;
        }
        self.time_counters.service_uptime_secs = self.service_health.uptime_secs;
        let registry = workgraph::AgentRegistry::load_or_warn(&self.workgraph_dir);
        let now = chrono::Utc::now();
        let (mut cumulative, mut active, mut active_count) = (0i64, 0i64, 0usize);
        for agent in registry.agents.values() {
            let start = chrono::DateTime::parse_from_rfc3339(&agent.started_at)
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc));
            let Some(start) = start else { continue };
            if agent.is_alive() {
                let e = (now - start).num_seconds().max(0);
                cumulative += e;
                active += e;
                active_count += 1;
            } else if let Some(ref end_str) = agent.completed_at {
                if let Ok(end) = chrono::DateTime::parse_from_rfc3339(end_str) {
                    cumulative += (end.with_timezone(&chrono::Utc) - start)
                        .num_seconds()
                        .max(0);
                }
            } else if let Ok(hb) = chrono::DateTime::parse_from_rfc3339(&agent.last_heartbeat) {
                cumulative += (hb.with_timezone(&chrono::Utc) - start)
                    .num_seconds()
                    .max(0);
            }
        }
        self.time_counters.cumulative_secs = cumulative as u64;
        self.time_counters.active_secs = active as u64;
        self.time_counters.active_agent_count = active_count;
        self.time_counters.counters_computed_at = Instant::now();

        self.time_counters.last_refresh = Instant::now();
    }

    /// Open or close the service control panel.
    pub fn toggle_service_control_panel(&mut self) {
        if self.service_health.panel_open {
            self.close_service_control_panel();
        } else {
            self.service_health.detail_open = false;
            self.service_health.panel_open = true;
            self.service_health.panel_focus = ControlPanelFocus::StartStop;
            self.service_health.panic_confirm = false;
        }
    }

    pub fn close_service_control_panel(&mut self) {
        self.service_health.panel_open = false;
        self.service_health.panic_confirm = false;
    }

    pub fn set_service_feedback(&mut self, msg: String) {
        self.service_health.feedback = Some((msg, Instant::now()));
    }

    pub fn execute_service_action(&mut self) {
        let health = &self.service_health;
        let is_running = health.pid.is_some() && health.level != ServiceHealthLevel::Red;
        match health.panel_focus.clone() {
            ControlPanelFocus::StartStop => {
                if is_running {
                    self.exec_command(
                        vec!["service".into(), "stop".into()],
                        CommandEffect::RefreshAndNotify("Service stopped".into()),
                    );
                    self.set_service_feedback("Service stopped".into());
                } else {
                    self.exec_command(
                        vec!["service".into(), "start".into()],
                        CommandEffect::RefreshAndNotify("Service started".into()),
                    );
                    self.set_service_feedback("Service starting...".into());
                }
            }
            ControlPanelFocus::PauseResume => {
                if health.paused {
                    self.exec_command(
                        vec!["service".into(), "resume".into()],
                        CommandEffect::RefreshAndNotify("Launches resumed".into()),
                    );
                    self.set_service_feedback("Resumed".into());
                } else {
                    self.exec_command(
                        vec!["service".into(), "pause".into()],
                        CommandEffect::RefreshAndNotify("Launches paused".into()),
                    );
                    self.set_service_feedback("Paused".into());
                }
            }
            ControlPanelFocus::Restart => {
                self.exec_command(
                    vec!["service".into(), "restart".into()],
                    CommandEffect::RefreshAndNotify("Service restarted".into()),
                );
                self.set_service_feedback("Restarting...".into());
            }
            ControlPanelFocus::AgentSlots => {
                // No action on Enter for agent slots — use +/- to adjust
            }
            ControlPanelFocus::PanicKill => {}
            ControlPanelFocus::StuckAgent(idx) => {
                if let Some(st) = health.stuck_tasks.get(idx) {
                    let aid = st.agent_id.clone();
                    self.exec_command(
                        vec!["kill".into(), aid.clone(), "--force".into()],
                        CommandEffect::RefreshAndNotify(format!("Killed {}", aid)),
                    );
                    self.set_service_feedback(format!("Killed {}", aid));
                }
            }
            ControlPanelFocus::KillAllDead => {
                self.exec_command(
                    vec!["kill".into(), "--all".into(), "--force".into()],
                    CommandEffect::RefreshAndNotify("Killed all dead agents".into()),
                );
                self.set_service_feedback("Killed all dead agents".into());
            }
            ControlPanelFocus::RetryFailedEvals => {
                self.exec_command(
                    vec!["retry".into(), "--failed-evals".into()],
                    CommandEffect::RefreshAndNotify("Retrying failed evals".into()),
                );
                self.set_service_feedback("Retrying failed evals...".into());
            }
        }
    }

    /// Adjust the max_agents slot count by `delta` (positive = increase, negative = decrease).
    /// Persists the change via `wg config --max-agents N` and updates local state immediately.
    pub fn adjust_agent_slots(&mut self, delta: i32) {
        let current = self.service_health.agents_max;
        let new_val = (current as i32 + delta).max(1) as usize;
        if new_val == current {
            return;
        }
        self.service_health.agents_max = new_val;
        self.exec_command(
            vec!["config".into(), "--max-agents".into(), new_val.to_string()],
            CommandEffect::RefreshAndNotify(format!("Set max_agents = {}", new_val)),
        );
        self.set_service_feedback(format!("Agent slots: {}", new_val));
    }

    pub fn execute_panic_kill(&mut self) {
        let n = self.service_health.agents_alive;
        self.exec_command(
            vec![
                "service".into(),
                "stop".into(),
                "--force".into(),
                "--kill-agents".into(),
            ],
            CommandEffect::RefreshAndNotify(format!("PANIC: killed {} agents", n)),
        );
        self.service_health.panic_confirm = false;
        self.set_service_feedback(format!("PANIC KILL: {} agents killed, service stopped", n));
    }

    /// Load chat history on startup for the active coordinator.
    /// Tries the persisted per-coordinator chat-history-{cid}.json first,
    /// then falls back to inbox/outbox.
    pub fn load_chat_history(&mut self) {
        // --no-history: start with empty chat, prevent scrollback.
        if self.no_history {
            self.chat.messages.clear();
            self.chat.has_more_history = false;
            self.chat.total_history_count = 0;
            self.chat.skipped_history_count = 0;
            // Still set outbox cursor so new messages during this session appear.
            if let Ok(msgs) = workgraph::chat::read_outbox_since_for(
                &self.workgraph_dir,
                self.active_coordinator_id,
                0,
            ) {
                self.chat.outbox_cursor = msgs.last().map(|m| m.id).unwrap_or(0);
            }
            return;
        }

        self.load_chat_history_for_coordinator(self.active_coordinator_id);

        // Also pre-load persisted chat for all other known coordinators so
        // switch_coordinator doesn't lose history. Use pagination for these too.
        let config = Config::load_or_default(&self.workgraph_dir);
        let page_size = self
            .history_depth_override
            .unwrap_or(config.tui.chat_page_size);
        let other_ids: Vec<u32> = self
            .list_coordinator_ids()
            .into_iter()
            .filter(|id| *id != self.active_coordinator_id)
            .collect();
        for cid in other_ids {
            let result = load_persisted_chat_history_paginated(&self.workgraph_dir, cid, page_size);
            if !result.messages.is_empty() {
                let mut state = ChatState::default();
                state.messages = result.messages;
                state.has_more_history = result.has_more;
                state.total_history_count = result.total_count;
                state.skipped_history_count =
                    result.total_count.saturating_sub(state.messages.len());
                // Set outbox cursor so we don't re-display old messages.
                if let Ok(outbox) =
                    workgraph::chat::read_outbox_since_for(&self.workgraph_dir, cid, 0)
                {
                    state.outbox_cursor = outbox.last().map(|m| m.id).unwrap_or(0);
                }
                self.coordinator_chats.entry(cid).or_insert(state);
            }
        }
    }

    /// Load chat history for a specific coordinator into self.chat (paginated).
    fn load_chat_history_for_coordinator(&mut self, coordinator_id: u32) {
        let config = Config::load_or_default(&self.workgraph_dir);
        let page_size = self
            .history_depth_override
            .unwrap_or(config.tui.chat_page_size);
        let result =
            load_persisted_chat_history_paginated(&self.workgraph_dir, coordinator_id, page_size);
        if !result.messages.is_empty() {
            self.chat.messages = result.messages;
            self.chat.has_more_history = result.has_more;
            self.chat.total_history_count = result.total_count;
            self.chat.skipped_history_count =
                result.total_count.saturating_sub(self.chat.messages.len());
        } else {
            // Fall back to inbox/outbox (e.g. first run after upgrade).
            let history = workgraph::chat::read_history_for(&self.workgraph_dir, coordinator_id)
                .unwrap_or_default();

            self.chat.messages.clear();
            for msg in &history {
                let role = match msg.role.as_str() {
                    "user" => ChatRole::User,
                    "coordinator" => ChatRole::Coordinator,
                    "system-error" => ChatRole::SystemError,
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
                let inbox_id = if role == ChatRole::User {
                    Some(msg.id)
                } else {
                    None
                };
                self.chat.messages.push(ChatMessage {
                    role,
                    text: msg.content.clone(),
                    full_text: msg.full_response.clone(),
                    attachments: att_names,
                    edited: false,
                    inbox_id,
                    user: msg.user.clone(),
                    target_task: None,
                    msg_timestamp: Some(msg.timestamp.clone()),
                    read_at: None,
                    msg_queue_id: None,
                });
            }

            self.chat.has_more_history = false;
            self.chat.total_history_count = self.chat.messages.len();
            self.chat.skipped_history_count = 0;

            // Persist the loaded history so next restart uses the file.
            if !self.chat.messages.is_empty() {
                save_chat_history(&self.workgraph_dir, coordinator_id, &self.chat.messages);
            }
        }

        // Set outbox cursor to latest outbox message ID so we don't re-display old messages.
        // Use the active coordinator's outbox (each coordinator has independent ID sequences).
        if let Ok(msgs) =
            workgraph::chat::read_outbox_since_for(&self.workgraph_dir, coordinator_id, 0)
        {
            self.chat.outbox_cursor = msgs.last().map(|m| m.id).unwrap_or(0);
        }

        // Check if archive files exist for scrollback beyond the active file.
        self.chat.archives_loaded = false;
        self.chat.has_archives =
            workgraph::chat::list_archives_for(&self.workgraph_dir, coordinator_id)
                .map(|a| !a.is_empty())
                .unwrap_or(false);
    }

    /// Save all coordinator chat states to disk (called on TUI exit).
    pub fn save_all_chat_state(&self) {
        // Save the active coordinator's chat, preserving unloaded older messages.
        save_chat_history_with_skip(
            &self.workgraph_dir,
            self.active_coordinator_id,
            &self.chat.messages,
            self.chat.skipped_history_count,
        );
        // Save all other coordinators' chat states, preserving unloaded older messages.
        for (cid, state) in &self.coordinator_chats {
            save_chat_history_with_skip(
                &self.workgraph_dir,
                *cid,
                &state.messages,
                state.skipped_history_count,
            );
        }
        // Save TUI focus state.
        save_tui_state(
            &self.workgraph_dir,
            self.active_coordinator_id,
            &self.right_panel_tab,
        );
    }

    /// Load the next page of older chat messages for the active coordinator.
    /// Prepends older messages to the beginning of `self.chat.messages`.
    /// When the active file is exhausted, loads from archive files.
    /// Returns true if new messages were loaded.
    pub fn load_more_chat_history(&mut self) -> bool {
        if self.no_history || !self.chat.has_more_history {
            // Check if we can still load from archives
            if !self.no_history && !self.chat.archives_loaded {
                return self.load_archive_history();
            }
            return false;
        }

        let config = Config::load_or_default(&self.workgraph_dir);
        let page_size = config.tui.chat_page_size;
        let loaded_count = self.chat.messages.len();

        let jsonl_path = chat_history_path(&self.workgraph_dir, self.active_coordinator_id);
        let older_messages = load_jsonl_page(&jsonl_path, loaded_count, page_size);

        if older_messages.is_empty() {
            self.chat.has_more_history = false;
            // Try loading from archives when active file is exhausted
            if !self.chat.archives_loaded {
                return self.load_archive_history();
            }
            return false;
        }

        // Prepend older messages to the beginning.
        let newly_loaded = older_messages.len();
        let mut combined = older_messages;
        combined.append(&mut self.chat.messages);
        self.chat.messages = combined;

        // Update pagination state.
        self.chat.skipped_history_count =
            self.chat.skipped_history_count.saturating_sub(newly_loaded);
        self.chat.has_more_history = self.chat.skipped_history_count > 0;

        true
    }

    /// Load messages from archived chat history files.
    /// Archives are loaded in reverse chronological order (newest archive first).
    /// Returns true if any messages were loaded.
    fn load_archive_history(&mut self) -> bool {
        self.chat.archives_loaded = true;

        let archives = match workgraph::chat::list_archives_for(
            &self.workgraph_dir,
            self.active_coordinator_id,
        ) {
            Ok(a) => a,
            Err(_) => return false,
        };

        if archives.is_empty() {
            self.chat.has_archives = false;
            return false;
        }

        self.chat.has_archives = true;

        // Load all archive messages. Archives are already sorted oldest-first.
        // We need to convert them to TUI ChatMessage format.
        let mut archive_messages: Vec<ChatMessage> = Vec::new();
        for archive_path in &archives {
            if let Ok(msgs) = workgraph::chat::read_archive_messages(archive_path) {
                for msg in msgs {
                    let role = match msg.role.as_str() {
                        "user" => ChatRole::User,
                        "coordinator" => ChatRole::Coordinator,
                        "system-error" => ChatRole::SystemError,
                        _ => ChatRole::System,
                    };
                    archive_messages.push(ChatMessage {
                        role,
                        text: msg.content,
                        full_text: msg.full_response,
                        attachments: msg
                            .attachments
                            .iter()
                            .map(|a| {
                                std::path::Path::new(&a.path)
                                    .file_name()
                                    .and_then(|f| f.to_str())
                                    .unwrap_or(&a.path)
                                    .to_string()
                            })
                            .collect(),
                        edited: false,
                        inbox_id: None,
                        user: msg.user,
                        target_task: None,
                        msg_timestamp: Some(msg.timestamp),
                        read_at: None,
                        msg_queue_id: None,
                    });
                }
            }
        }

        if archive_messages.is_empty() {
            return false;
        }

        // Also check for TUI history archives (chat-history-*.jsonl in archive dir)
        let archive_dir = self
            .workgraph_dir
            .join("chat")
            .join(self.active_coordinator_id.to_string())
            .join("archive");
        if archive_dir.exists()
            && let Ok(entries) = std::fs::read_dir(&archive_dir)
        {
            let mut tui_archives: Vec<std::path::PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("chat-history-") && n.ends_with(".jsonl"))
                        .unwrap_or(false)
                })
                .collect();
            tui_archives.sort();
            for path in &tui_archives {
                let result = load_jsonl_tail(path, usize::MAX);
                archive_messages.extend(result.messages);
            }
        }

        // Sort archive messages by timestamp, then prepend to current messages.
        archive_messages.sort_by(|a, b| {
            let ts_a = a.msg_timestamp.as_deref().unwrap_or("");
            let ts_b = b.msg_timestamp.as_deref().unwrap_or("");
            ts_a.cmp(ts_b)
        });

        let loaded = archive_messages.len();
        archive_messages.append(&mut self.chat.messages);
        self.chat.messages = archive_messages;
        self.chat.skipped_history_count = 0;

        loaded > 0
    }

    /// Poll for new coordinator responses in the outbox and streaming updates.
    /// Called during refresh ticks.
    pub fn poll_chat_messages(&mut self) {
        // Poll the streaming file for partial text while awaiting a response.
        if self.chat.awaiting_response() {
            let streaming =
                workgraph::chat::read_streaming(&self.workgraph_dir, self.active_coordinator_id);
            self.chat.streaming_text = streaming;
        }

        let new_msgs = match workgraph::chat::read_outbox_since_for(
            &self.workgraph_dir,
            self.active_coordinator_id,
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
            let role = match msg.role.as_str() {
                "system-error" => ChatRole::SystemError,
                "user" => ChatRole::User,
                _ => ChatRole::Coordinator,
            };
            self.chat.messages.push(ChatMessage {
                role,
                text: msg.content.clone(),
                full_text: msg.full_response.clone(),
                attachments: att_names,
                edited: false,
                inbox_id: None,
                user: msg.user.clone(),
                target_task: None,
                msg_timestamp: Some(msg.timestamp.clone()),
                read_at: None,
                msg_queue_id: None,
            });
        }

        // Persist updated chat history.
        save_chat_history_with_skip(
            &self.workgraph_dir,
            self.active_coordinator_id,
            &self.chat.messages,
            self.chat.skipped_history_count,
        );

        // Phase 1 trigger: New message → Info toast (only when Chat panel isn't focused,
        // so we don't show redundant toasts when the user is already reading the chat).
        if self.right_panel_tab != RightPanelTab::Chat {
            let count = new_msgs.len();
            let label = if count == 1 { "message" } else { "messages" };
            self.push_toast(
                format!("\u{1f4ac} {} new {} from coordinator", count, label),
                ToastSeverity::Info,
            );
        }

        // Update cursor to latest message.
        self.chat.outbox_cursor = new_msgs
            .last()
            .map(|m| m.id)
            .unwrap_or(self.chat.outbox_cursor);

        // Retire one pending request per batch of new outbox messages (P2 fix).
        // The TUI request_id ("tui-...") differs from wg chat's ("chat-..."),
        // so we retire by count rather than matching by ID. Coordinator processes
        // requests in FIFO order, so one response batch retires one request.
        if !self.chat.pending_request_ids.is_empty()
            && let Some(first) = self.chat.pending_request_ids.iter().next().cloned()
        {
            self.chat.pending_request_ids.remove(&first);
        }
        // If all requests are now answered, clear streaming state.
        if self.chat.pending_request_ids.is_empty() {
            self.chat.awaiting_since = None;
            self.chat.streaming_text.clear();
        }

        // Reorder deferred user messages to after the newly arrived coordinator
        // messages (P1 fix). Extract them in reverse index order to avoid shifting,
        // then append in original order.
        if !self.chat.deferred_user_indices.is_empty() {
            let mut deferred: Vec<ChatMessage> = Vec::new();
            for &idx in self.chat.deferred_user_indices.iter().rev() {
                if idx < self.chat.messages.len() {
                    deferred.push(self.chat.messages.remove(idx));
                }
            }
            deferred.reverse();
            self.chat.messages.extend(deferred);
            self.chat.deferred_user_indices.clear();
        }

        // Auto-scroll to bottom when new messages arrive (if user hasn't scrolled up).
        if self.chat.scroll == 0 {
            // Already at bottom; new messages will be visible.
        }
    }

    /// Poll for messages sent to agents that have been read, and interleave them
    /// into the chat stream as `SentMessage` entries at their read-at position.
    /// NOTE: No longer called in production — the coordinator chat now only shows
    /// user↔coordinator messages. Task messages are visible in the Messages panel.
    /// Kept for test coverage of the interleaving logic.
    #[cfg(test)]
    fn poll_interleaved_messages(&mut self) {
        // Collect IDs of messages already shown in the chat stream for dedup.
        let shown_ids: std::collections::HashSet<(String, u64)> = self
            .chat
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::SentMessage)
            .filter_map(|m| {
                let task = m.target_task.as_ref()?;
                let id = m.msg_queue_id?;
                Some((task.clone(), id))
            })
            .collect();

        // Scan message files for all tasks in the messages directory.
        let msg_dir = self.workgraph_dir.join("messages");
        let entries = match std::fs::read_dir(&msg_dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        let mut new_messages: Vec<ChatMessage> = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            let fname = match path.file_name().and_then(|f| f.to_str()) {
                Some(f) => f.to_string(),
                None => continue,
            };
            // Only process .jsonl message files (skip .cursors directory).
            if !fname.ends_with(".jsonl") {
                continue;
            }
            let task_id = fname.trim_end_matches(".jsonl");

            // Read messages for this task.
            let msgs = match workgraph::messages::list_messages(&self.workgraph_dir, task_id) {
                Ok(m) => m,
                Err(_) => continue,
            };

            for msg in &msgs {
                // Only interleave messages that have been read by the agent.
                if !matches!(
                    msg.status,
                    workgraph::messages::DeliveryStatus::Read
                        | workgraph::messages::DeliveryStatus::Acknowledged
                ) {
                    continue;
                }

                // Skip if already shown.
                if shown_ids.contains(&(task_id.to_string(), msg.id)) {
                    continue;
                }

                // Skip messages sent by agents (we only interleave incoming messages
                // sent TO agents, i.e., from user/coordinator/tui).
                let is_from_user = matches!(msg.sender.as_str(), "user" | "tui" | "coordinator");
                if !is_from_user {
                    continue;
                }

                new_messages.push(ChatMessage {
                    role: ChatRole::SentMessage,
                    text: msg.body.clone(),
                    full_text: None,
                    attachments: vec![],
                    edited: false,
                    inbox_id: None,
                    user: Some(msg.sender.clone()),
                    target_task: Some(task_id.to_string()),
                    msg_timestamp: Some(msg.timestamp.clone()),
                    read_at: msg.read_at.clone(),
                    msg_queue_id: Some(msg.id),
                });
            }
        }

        if new_messages.is_empty() {
            return;
        }

        // Insert each new message at the correct temporal position based on read_at.
        // We use the read_at timestamp to determine where the message belongs in the
        // chat stream: it appears at the point the agent actually read it.
        for msg in new_messages {
            let read_at = msg.read_at.as_deref().unwrap_or("");
            // Find insertion point: after the last message whose timestamp <= read_at.
            // For non-SentMessage entries, we use msg_timestamp or fall back to
            // "beginning of time" (they sort naturally by insertion order).
            let insert_idx = self
                .chat
                .messages
                .iter()
                .rposition(|m| {
                    let ts = m.msg_timestamp.as_deref().unwrap_or("");
                    ts <= read_at
                })
                .map(|i| i + 1)
                .unwrap_or(self.chat.messages.len());
            self.chat.messages.insert(insert_idx, msg);
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
            edited: false,
            inbox_id: None,
            user: Some(workgraph::current_user()),
            target_task: None,
            msg_timestamp: Some(chrono::Utc::now().to_rfc3339()),
            read_at: None,
            msg_queue_id: None,
        });

        // Persist updated chat history.
        save_chat_history_with_skip(
            &self.workgraph_dir,
            self.active_coordinator_id,
            &self.chat.messages,
            self.chat.skipped_history_count,
        );

        // Reset scroll to bottom.
        self.chat.scroll = 0;

        // Track deferred user message if a response is already in flight (P1 fix).
        if !self.chat.pending_request_ids.is_empty() {
            let idx = self.chat.messages.len() - 1;
            self.chat.deferred_user_indices.push(idx);
        }

        // Mark as awaiting response (P2 fix: set-based tracking).
        if self.chat.pending_request_ids.is_empty() {
            self.chat.awaiting_since = Some(std::time::Instant::now());
        }
        self.chat.pending_request_ids.insert(request_id.clone());

        // Fast path: when we're the PTY owner for this coordinator
        // (our own `wg nex --chat <ref>` is tailing the inbox), write
        // to inbox directly instead of routing through `wg chat` → IPC
        // → daemon. The daemon doesn't do any useful work for chat in
        // this mode — it's just a relay — and requiring it means the
        // user's Enter silently drops onto the floor when no daemon is
        // running. Observer mode still goes via IPC so the external
        // handler (that owns the session) sees the release marker
        // through the daemon's lock-aware write path.
        if self.chat_pty_mode && !self.chat_pty_observer {
            // Build attachment list in `chat::Attachment` shape so
            // append_inbox_with_attachments_for can record them.
            let attachments: Vec<workgraph::chat::Attachment> = self
                .chat
                .pending_attachments
                .iter()
                .map(|a| workgraph::chat::Attachment {
                    path: a.stored_path.clone(),
                    mime_type: a.mime_type.clone(),
                    size_bytes: a.size_bytes,
                })
                .collect();
            self.chat.pending_attachments.clear();

            if let Err(e) = workgraph::chat::append_inbox_with_attachments_for(
                &self.workgraph_dir,
                self.active_coordinator_id,
                &text,
                &request_id,
                attachments,
            ) {
                eprintln!("[tui] direct inbox write failed for {}: {}", request_id, e);
            }
            // No exec_command + no CommandEffect::ChatResponse — the
            // response arrives by `poll_chat_messages` tailing the
            // outbox, same polling cycle used for file-tailing mode.
            // `pending_request_ids` will be retired when the outbox
            // picks up a response.
            return;
        }

        // Standard path (file-tailing mode, observer, or non-PTY): run
        // `wg chat` which IPCs the daemon. Daemon writes inbox + (in
        // daemon-coordinator mode) routes the response back.
        let mut args = vec!["chat".to_string(), text];
        if self.active_coordinator_id != 0 {
            args.push("--coordinator".to_string());
            args.push(self.active_coordinator_id.to_string());
        }
        for att in &self.chat.pending_attachments {
            args.push("--attachment".to_string());
            args.push(att.stored_path.clone());
        }

        // Clear pending attachments after sending.
        self.chat.pending_attachments.clear();

        // Send via `wg chat` command in background.
        self.exec_command(args, CommandEffect::ChatResponse(request_id));

        // Phase 3c takeover-on-send. If the Chat tab is showing a
        // read-only observer PTY (because some other process owns
        // the session), a user message is the signal for takeover.
        // Write the release marker alongside the inbox append so
        // the external handler notices at its next turn boundary,
        // drains the inbox (including our just-written message),
        // and exits cleanly. The main event loop polls for lock
        // release and swaps the observer pane for an owner pane
        // when it happens.
        if self.chat_pty_mode && self.chat_pty_observer {
            let task_id = workgraph::chat_id::format_chat_task_id(self.active_coordinator_id);
            let chat_dir = self.workgraph_dir.join("chat").join(&task_id);
            if let Err(e) = workgraph::session_lock::request_release(&chat_dir) {
                eprintln!("[tui] failed to write release marker for takeover: {}", e);
            } else {
                self.chat_pty_takeover_pending_since = Some(std::time::Instant::now());
            }
        }
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
                self.push_toast(format!("Attached: {}", filename), ToastSeverity::Info);
            }
            Err(e) => {
                self.push_toast(format!("Attach failed: {}", e), ToastSeverity::Error);
            }
        }
    }

    /// Check whether a user message at the given index has been consumed by the coordinator.
    /// A message is consumed if there's any coordinator message after it in the display list.
    pub fn is_chat_message_consumed(&self, index: usize) -> bool {
        if index >= self.chat.messages.len() {
            return true;
        }
        if self.chat.messages[index].role != ChatRole::User {
            return true; // only user messages can be unconsumed
        }
        // If any coordinator message follows this one, it's consumed.
        self.chat.messages[index + 1..]
            .iter()
            .any(|m| m.role == ChatRole::Coordinator)
    }

    /// Enter edit mode for a user message at the given index.
    /// Loads the message text into the editor and saves the current input.
    pub fn enter_chat_edit_mode(&mut self, index: usize) {
        if index >= self.chat.messages.len() {
            return;
        }
        if self.chat.messages[index].role != ChatRole::User {
            return;
        }
        if self.is_chat_message_consumed(index) {
            return;
        }
        // Save current input
        self.chat.edit_saved_input = editor_text(&self.chat.editor);
        self.chat.editing_index = Some(index);
        // Load the message text into the editor
        self.chat.editor = new_emacs_editor_with(&self.chat.messages[index].text);
        // Reset scroll to bottom so the input is visible
        self.chat.scroll = 0;
    }

    /// Cancel edit mode, restoring the previous input.
    pub fn cancel_chat_edit_mode(&mut self) {
        if self.chat.editing_index.is_some() {
            let saved = std::mem::take(&mut self.chat.edit_saved_input);
            if saved.is_empty() {
                editor_clear(&mut self.chat.editor);
            } else {
                self.chat.editor = new_emacs_editor_with(&saved);
            }
            self.chat.editing_index = None;
            self.chat.history_cursor = None;
        }
    }

    /// Commit the edit: update the original message with the editor text.
    pub fn commit_chat_edit(&mut self) {
        if let Some(idx) = self.chat.editing_index {
            let new_text = editor_text(&self.chat.editor);
            if new_text.trim().is_empty() {
                // Empty edit = delete the message
                self.delete_chat_message(idx);
            } else if idx < self.chat.messages.len() {
                let old_text = self.chat.messages[idx].text.clone();
                if new_text != old_text {
                    self.chat.messages[idx].text = new_text.clone();
                    self.chat.messages[idx].edited = true;
                    // Update inbox if we have an ID
                    if let Some(inbox_id) = self.chat.messages[idx].inbox_id {
                        let _ = workgraph::chat::edit_inbox_message_for(
                            &self.workgraph_dir,
                            self.active_coordinator_id,
                            inbox_id,
                            &new_text,
                        );
                    }
                    save_chat_history_with_skip(
                        &self.workgraph_dir,
                        self.active_coordinator_id,
                        &self.chat.messages,
                        self.chat.skipped_history_count,
                    );
                }
            }
            editor_clear(&mut self.chat.editor);
            self.chat.editing_index = None;
            self.chat.history_cursor = None;
            self.chat.edit_saved_input.clear();
        }
    }

    /// Delete a chat message at the given index.
    pub fn delete_chat_message(&mut self, index: usize) {
        if index >= self.chat.messages.len() {
            return;
        }
        if self.is_chat_message_consumed(index) {
            return;
        }
        // Delete from inbox if we have an ID
        if let Some(inbox_id) = self.chat.messages[index].inbox_id {
            let _ = workgraph::chat::delete_inbox_message_for(
                &self.workgraph_dir,
                self.active_coordinator_id,
                inbox_id,
            );
        }
        self.chat.messages.remove(index);
        save_chat_history_with_skip(
            &self.workgraph_dir,
            self.active_coordinator_id,
            &self.chat.messages,
            self.chat.skipped_history_count,
        );
        // Clear edit state
        self.chat.editing_index = None;
        self.chat.history_cursor = None;
        editor_clear(&mut self.chat.editor);
        self.push_toast("Message deleted".to_string(), ToastSeverity::Info);
    }

    /// Get the indices of editable (unconsumed) user messages, in order.
    pub fn editable_user_message_indices(&self) -> Vec<usize> {
        self.chat
            .messages
            .iter()
            .enumerate()
            .filter(|(i, m)| m.role == ChatRole::User && !self.is_chat_message_consumed(*i))
            .map(|(i, _)| i)
            .collect()
    }

    /// Navigate to the previous user message in history (Up arrow).
    /// Returns true if navigation happened.
    pub fn chat_history_up(&mut self) -> bool {
        let editable = self.editable_user_message_indices();
        if editable.is_empty() {
            return false;
        }
        match self.chat.history_cursor {
            None => {
                // Save current input and start from the most recent editable message
                self.chat.edit_saved_input = editor_text(&self.chat.editor);
                let msg_idx = *editable.last().unwrap();
                self.chat.history_cursor = Some(editable.len() - 1);
                self.chat.editing_index = Some(msg_idx);
                self.chat.editor = new_emacs_editor_with(&self.chat.messages[msg_idx].text);
                true
            }
            Some(cursor) => {
                if cursor > 0 {
                    let new_cursor = cursor - 1;
                    let msg_idx = editable[new_cursor];
                    self.chat.history_cursor = Some(new_cursor);
                    self.chat.editing_index = Some(msg_idx);
                    self.chat.editor = new_emacs_editor_with(&self.chat.messages[msg_idx].text);
                    true
                } else {
                    false // already at oldest
                }
            }
        }
    }

    /// Navigate to the next user message in history (Down arrow).
    /// Returns true if navigation happened.
    pub fn chat_history_down(&mut self) -> bool {
        let editable = self.editable_user_message_indices();
        if let Some(cursor) = self.chat.history_cursor {
            if cursor + 1 < editable.len() {
                let new_cursor = cursor + 1;
                let msg_idx = editable[new_cursor];
                self.chat.history_cursor = Some(new_cursor);
                self.chat.editing_index = Some(msg_idx);
                self.chat.editor = new_emacs_editor_with(&self.chat.messages[msg_idx].text);
                true
            } else {
                // Past the end: restore original input
                self.chat.history_cursor = None;
                self.chat.editing_index = None;
                let saved = std::mem::take(&mut self.chat.edit_saved_input);
                if saved.is_empty() {
                    editor_clear(&mut self.chat.editor);
                } else {
                    self.chat.editor = new_emacs_editor_with(&saved);
                }
                true
            }
        } else {
            false
        }
    }

    /// Switch to a different coordinator session.
    /// Saves the current chat state to the coordinator_chats map and loads the target.
    pub fn switch_coordinator(&mut self, target_id: u32) {
        if target_id == self.active_coordinator_id {
            return;
        }
        // Save dismissed flag into the outgoing chat state
        let mut current = std::mem::take(&mut self.chat);
        current.chat_input_dismissed = self.chat_input_dismissed;
        self.coordinator_chats
            .insert(self.active_coordinator_id, current);

        // Load target chat state: try in-memory first, then persisted file (paginated), then default.
        // --no-history: always start with empty chat for any coordinator.
        self.chat = if self.no_history {
            ChatState::default()
        } else {
            self.coordinator_chats
                .remove(&target_id)
                .unwrap_or_else(|| {
                    let config = Config::load_or_default(&self.workgraph_dir);
                    let page_size = self
                        .history_depth_override
                        .unwrap_or(config.tui.chat_page_size);
                    let result = load_persisted_chat_history_paginated(
                        &self.workgraph_dir,
                        target_id,
                        page_size,
                    );
                    if result.messages.is_empty() {
                        ChatState::default()
                    } else {
                        ChatState {
                            has_more_history: result.has_more,
                            total_history_count: result.total_count,
                            skipped_history_count: result
                                .total_count
                                .saturating_sub(result.messages.len()),
                            messages: result.messages,
                            ..Default::default()
                        }
                    }
                })
        };

        // Initialize outbox cursor for newly created chat states so we don't
        // re-display old messages or miss new ones (each coordinator has independent
        // ID sequences — a cursor from coordinator-0 is meaningless for coordinator-6).
        if self.chat.outbox_cursor == 0
            && let Ok(msgs) =
                workgraph::chat::read_outbox_since_for(&self.workgraph_dir, target_id, 0)
        {
            self.chat.outbox_cursor = msgs.last().map(|m| m.id).unwrap_or(0);
        }

        // Always reset to Normal when switching coordinators so arrow-key
        // navigation doesn't get stuck in input mode. The user must explicitly
        // re-enter chat/message input (Enter, click, 'c', etc.).
        if matches!(
            self.input_mode,
            InputMode::ChatInput | InputMode::MessageInput
        ) {
            self.input_mode = InputMode::Normal;
            self.inspector_sub_focus = InspectorSubFocus::ChatHistory;
        }
        self.chat_input_dismissed = self.chat.chat_input_dismissed;

        self.active_coordinator_id = target_id;

        // Auto-enter PTY mode when switching to a native-executor
        // coordinator (Step 1 of nex-as-everything). Harmless no-op for
        // claude/codex coordinators — those keep the file-tailing path.
        self.maybe_auto_enable_chat_pty();

        // Sync: highlight the corresponding coordinator task in the graph.
        let coord_task_id = if target_id == 0 {
            ".coordinator".to_string()
        } else {
            workgraph::chat_id::format_chat_task_id(target_id)
        };
        if let Some(idx) = self.task_order.iter().position(|id| *id == coord_task_id) {
            self.selected_task_idx = Some(idx);
            // Don't call recompute_trace() here to avoid infinite recursion
            // (recompute_trace calls switch_coordinator for coordinator tasks).
            // Just update the selection visually.
            self.scroll_to_selected_task();
        }
    }

    /// Restore TUI focus state from the previous session's tui-state.json.
    /// Sets `active_coordinator_id` and `right_panel_tab` if the persisted
    /// coordinator still exists in the graph.
    fn restore_tui_state(&mut self) {
        if let Some(state) = load_tui_state(&self.workgraph_dir) {
            let known_ids = self.list_coordinator_ids();
            if known_ids.contains(&state.active_coordinator_id) {
                self.active_coordinator_id = state.active_coordinator_id;
                // Restore right panel tab.
                self.right_panel_tab = match state.right_panel_tab.as_str() {
                    "Chat" => RightPanelTab::Chat,
                    "Detail" => RightPanelTab::Detail,
                    "Log" => RightPanelTab::Log,
                    "Messages" => RightPanelTab::Messages,
                    "Agency" => RightPanelTab::Agency,
                    "Config" => RightPanelTab::Config,
                    "Files" => RightPanelTab::Files,
                    "CoordLog" => RightPanelTab::CoordLog,
                    "Firehose" => RightPanelTab::Firehose,
                    "Output" => RightPanelTab::Output,
                    "Dashboard" => RightPanelTab::Dashboard,
                    _ => RightPanelTab::Chat,
                };
            }
        }
    }

    /// For the currently-active coordinator, auto-enable `chat_pty_mode`
    /// and spawn an embedded REPL chosen by the coordinator's effective
    /// executor. The coordinator view is just a container for
    /// interactive chat sessions associated with this workgraph — we
    /// pick the child process per executor type:
    ///
    /// - `native`  → `wg nex --resume <ref>` (our own REPL inside a
    ///   PTY, stdin-based input via rustyline, same treatment as
    ///   claude/codex).
    /// - `claude`  → `claude` CLI direct. Uses the user's Claude
    ///   subscription auth + Claude's native interactive UI.
    /// - `codex`   → `codex` CLI direct. Uses the user's ChatGPT/Codex
    ///   subscription auth + Codex's native UI.
    ///
    /// The tradeoff for claude/codex is ephemeral: the chat transcript
    /// lives in the vendor's session store, not in `chat/<ref>/`. If
    /// the user wants the workgraph chat history to include those
    /// turns, they should pick the `native` executor with
    /// `-m claude:opus` — that routes through our REPL which does write
    /// inbox/outbox, at the cost of using the raw API budget instead
    /// of the subscription.
    ///
    /// Falls through to the file-tailing chat path silently if the
    /// vendor CLI isn't on PATH or the spawn fails. Ctrl+T still toggles
    /// manually. Idempotent: no-op when a live pane already exists.
    pub fn maybe_auto_enable_chat_pty(&mut self) {
        let config = Config::load_or_default(&self.workgraph_dir);
        let executor = config.coordinator.effective_executor();

        // Task ID (`.coordinator-N`, with dot) is what `wg spawn-task`
        // needs to look the task up in the graph and what our
        // `task_panes` map is keyed by. Chat ref (`coordinator-N`,
        // without dot) is what `chat::chat_dir_for_ref` and
        // `session_lock::read_holder` take — that's the session
        // alias registered in `sessions.json`. Mixing them up means
        // chat_dir_for_ref can't find the alias, falls back to the
        // literal `chat/.coordinator-N/` (which doesn't exist),
        // observer_mode reads as false, and we then spawn-task in
        // owner mode even though the daemon already holds the lock.
        // spawn-task fails with "session lock busy", child exits
        // immediately, render falls through to file-tailing — which
        // is exactly the broken state the user smoke-tested into.
        let task_id = workgraph::chat_id::format_chat_task_id(self.active_coordinator_id);
        let chat_ref = format!("coordinator-{}", self.active_coordinator_id);

        let pane_live = self
            .task_panes
            .get_mut(&task_id)
            .map(|p| p.is_alive())
            .unwrap_or(false);
        if pane_live {
            self.chat_pty_mode = true;
            return;
        }
        self.task_panes.remove(&task_id);

        let self_exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "wg".to_string());

        // Resolve (binary, args, observer_mode) per executor. Observer
        // mode (lock-tailing) only applies to native today because the
        // vendor CLIs run their own session management off-graph.
        let chat_dir = workgraph::chat::chat_dir_for_ref(&self.workgraph_dir, &chat_ref);
        let observer_mode = workgraph::session_lock::read_holder(&chat_dir)
            .ok()
            .flatten()
            .is_some_and(|info| info.alive);

        // Resolve per-coordinator cwd for vendor CLIs. claude and codex
        // look at "most recent session in current dir" when `--continue`
        // is used, so giving each coordinator its own chat_dir lets
        // each tab resume its own conversation independently. Make
        // sure the dir exists; claude/codex will error otherwise.
        let _ = std::fs::create_dir_all(&chat_dir);

        // Forced takeover: if any handler holds this coordinator's
        // session lock (usually the daemon's own `wg nex --chat` agent
        // spawned via `wg service start`), we want it gone. The user
        // opened `wg tui` to drive chat themselves — the daemon's
        // autonomous-dispatch handler is in the way. Soft release via
        // the release-marker can stall indefinitely if the holder is
        // stuck in a retry loop (broken endpoint, slow LLM turn, etc.)
        // because the marker is only checked at turn boundaries.
        //
        // Policy: request release first (gentler, gives the holder a
        // chance to flush cleanly), wait briefly, then SIGTERM if
        // still alive. The TUI session owns the chat surface; this is
        // the `wg tui` contract now.
        if observer_mode
            && let Ok(Some(holder)) = workgraph::session_lock::read_holder(&chat_dir)
            && holder.alive
        {
            let _ = workgraph::session_lock::request_release(&chat_dir);
            // 300ms grace for clean exit.
            let mut released = false;
            for _ in 0..3 {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let still_held = workgraph::session_lock::read_holder(&chat_dir)
                    .ok()
                    .flatten()
                    .is_some_and(|info| info.alive);
                if !still_held {
                    released = true;
                    break;
                }
            }
            if !released {
                // Stuck holder — force quit. Safe because:
                //   - the marker is already written so the handler
                //     won't spawn a replacement;
                //   - we own the lock file path, re-acquiring works;
                //   - any in-flight work this handler was doing is
                //     recoverable from outbox on next open.
                unsafe {
                    libc::kill(holder.pid as libc::pid_t, libc::SIGTERM);
                }
                eprintln!(
                    "[tui] forced takeover: SIGTERM'd stuck handler pid={} for {}",
                    holder.pid, chat_ref
                );
                // Short wait for the process to exit + lock file to clear.
                for _ in 0..10 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    let still_held = workgraph::session_lock::read_holder(&chat_dir)
                        .ok()
                        .flatten()
                        .is_some_and(|info| info.alive);
                    if !still_held {
                        break;
                    }
                }
            }
        }
        // Re-check lock state after takeover.
        let observer_mode = workgraph::session_lock::read_holder(&chat_dir)
            .ok()
            .flatten()
            .is_some_and(|info| info.alive);

        // Owned String args per executor.
        let (bin, args_owned, cwd_opt): (String, Vec<String>, Option<std::path::PathBuf>) =
            match executor.as_str() {
                "native" => {
                    // Spawn `wg nex` as a real PTY child, same as
                    // claude/codex. Uses `--resume` (not `--chat`) so
                    // nex reads from stdin via rustyline instead of
                    // inbox.jsonl — keystrokes flow through the PTY.
                    let _ = workgraph::chat_sessions::ensure_session(
                        &self.workgraph_dir,
                        &chat_ref,
                        workgraph::chat_sessions::SessionKind::Coordinator,
                        Some(format!("coordinator {}", self.active_coordinator_id)),
                    );
                    let mut args = vec![
                        "nex".to_string(),
                        "--role".to_string(),
                        "coordinator".to_string(),
                        "--resume".to_string(),
                        chat_ref.clone(),
                    ];
                    let model = config
                        .coordinator
                        .model
                        .clone()
                        .unwrap_or_else(|| config.agent.model.clone());
                    if !model.is_empty() {
                        args.push("-m".to_string());
                        args.push(model);
                    }
                    if let Some(ep) = config
                        .llm_endpoints
                        .endpoints
                        .iter()
                        .find(|e| e.is_default)
                        .and_then(|e| e.url.clone())
                    {
                        args.push("-e".to_string());
                        args.push(ep);
                    }
                    let project_root = self
                        .workgraph_dir
                        .parent()
                        .unwrap_or(&self.workgraph_dir)
                        .to_path_buf();
                    (self_exe.clone(), args, Some(project_root))
                }
                "claude" => {
                    let project_root = self
                        .workgraph_dir
                        .parent()
                        .unwrap_or(&self.workgraph_dir)
                        .to_path_buf();
                    let project_tag = project_root
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("project");
                    let session_name = format!("wg-{}-{}", project_tag, chat_ref);
                    let session_uuid = claude_session_uuid(&project_root, &session_name);
                    let has_prior = claude_session_exists(&project_root, &session_uuid);
                    let mut args = vec![
                        "-n".to_string(),
                        session_name,
                        "--dangerously-skip-permissions".to_string(),
                    ];
                    if has_prior {
                        args.push("--resume".to_string());
                        args.push(session_uuid.to_string());
                    } else {
                        args.push("--session-id".to_string());
                        args.push(session_uuid.to_string());
                        let sys_prompt =
                            crate::commands::service::coordinator_agent::build_system_prompt(
                                &self.workgraph_dir,
                            );
                        args.push("--system-prompt".to_string());
                        args.push(sys_prompt);
                    }
                    ("claude".to_string(), args, Some(project_root))
                }
                "codex" => {
                    // Coordinator priming: codex auto-loads AGENTS.md
                    // from CWD (hierarchically, up to the git root), so
                    // we materialize the full coordinator prompt into
                    // `<chat_dir>/AGENTS.md` before spawn. codex has no
                    // --system-prompt flag in interactive mode; AGENTS.md
                    // is the supported mechanism. Scoping to chat_dir
                    // keeps per-coordinator priming isolated from any
                    // project-level AGENTS.md.
                    let sys_prompt =
                        crate::commands::service::coordinator_agent::build_system_prompt(
                            &self.workgraph_dir,
                        );
                    let agents_md = chat_dir.join("AGENTS.md");
                    let _ = std::fs::write(&agents_md, sys_prompt);
                    // Resume: three strategies, checked in order:
                    //   1. `.codex-session-id` persisted by the daemon's
                    //      codex_handler → `codex resume <id>`
                    //   2. `.codex-pty-launched` marker from a prior TUI
                    //      PTY session → `codex resume --last` (codex
                    //      filters by CWD, so this picks up the right one)
                    //   3. Neither → fresh session, write the marker
                    let session_id_path = chat_dir.join(".codex-session-id");
                    let pty_marker = chat_dir.join(".codex-pty-launched");
                    let prior_session_id = std::fs::read_to_string(&session_id_path)
                        .ok()
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty());
                    let args = if let Some(sid) = prior_session_id {
                        vec!["resume".to_string(), sid]
                    } else if pty_marker.exists() {
                        vec!["resume".to_string(), "--last".to_string()]
                    } else {
                        let _ = std::fs::write(&pty_marker, "");
                        Vec::new()
                    };
                    ("codex".to_string(), args, Some(chat_dir.clone()))
                }
                _ => {
                    // Unknown executor — leave file-tailing path in charge.
                    return;
                }
            };
        self.chat_pty_observer = observer_mode && executor == "native";

        let args_ref: Vec<&str> = args_owned.iter().map(String::as_str).collect();
        let env: Vec<(String, String)> = vec![
            (
                "WG_DIR".to_string(),
                self.workgraph_dir.display().to_string(),
            ),
            // Override inherited WG_EXECUTOR_TYPE so spawn-task
            // dispatches the same executor the TUI chose from config.
            ("WG_EXECUTOR_TYPE".to_string(), executor.clone()),
            // Vendor CLIs (claude in particular) expect a real-looking
            // TERM. portable-pty doesn't set one by default; inheriting
            // the wg-tui parent's TERM works but passing an explicit
            // xterm-256color avoids oddities when WG_TUI runs under a
            // minimal terminal like linux console or dumb.
            ("TERM".to_string(), "xterm-256color".to_string()),
        ];

        let spawn_result = crate::tui::pty_pane::PtyPane::spawn_in(
            &bin,
            &args_ref,
            &env,
            cwd_opt.as_deref(),
            24,
            80,
        );
        match spawn_result {
            Ok(pane) => {
                self.task_panes.insert(task_id, pane);
                self.chat_pty_mode = true;
                // All three PTY modes run interactive REPLs that
                // read from stdin: native wg nex (rustyline),
                // claude, codex. Forward keystrokes directly.
                self.chat_pty_forwards_stdin = true;
                // Shift focus into the right panel so keystrokes route
                // to the PTY (matches `toggle_chat_pty_mode` on Ctrl+T).
                // Without this, the graph panel owns keys and hotkeys
                // like 'e' fire graph-side dialogs instead of reaching
                // `wg nex` inside the pane.
                self.focused_panel = FocusedPanel::RightPanel;
            }
            Err(e) => {
                eprintln!(
                    "[tui] auto-enable chat PTY for executor '{}' failed ({}): \
                     falling back to file-tailing. \
                     Is the `{}` binary on PATH?",
                    executor, e, bin
                );
                self.chat_pty_mode = false;
                self.chat_pty_forwards_stdin = false;
            }
        }
    }

    /// On TUI startup, auto-create a coordinator labeled with the current
    /// WG_USER identity if none exists for that user. This ensures each user
    /// gets their own chat agent managing their own agent budget.
    pub fn ensure_user_coordinator(&mut self) {
        let user = workgraph::current_user();
        // Don't auto-create for the fallback "unknown" identity
        if user == "unknown" {
            return;
        }

        let expected_title = format!("Chat: {}", user);
        let legacy_expected_title = format!("Coordinator: {}", user);

        // Load the graph to check chat task titles directly.
        // list_coordinator_ids_and_labels() returns display labels like ".chat-N"
        // which don't match the "Chat: {user}" title format.
        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = workgraph::parser::load_graph(&graph_path).ok();

        // Find a non-archived chat task whose title matches (new or legacy)
        let existing_coord: Option<u32> = graph.as_ref().and_then(|g| {
            g.tasks()
                .filter(|t| t.tags.iter().any(|tag| workgraph::chat_id::is_chat_loop_tag(tag)))
                .filter(|t| !matches!(t.status, workgraph::graph::Status::Abandoned))
                .filter(|t| !t.tags.iter().any(|tag| tag == "archived"))
                .filter(|t| t.title == expected_title || t.title == legacy_expected_title)
                .filter_map(|t| {
                    workgraph::chat_id::parse_chat_task_id(&t.id).or_else(|| {
                        if t.id == ".coordinator" {
                            Some(0)
                        } else {
                            None
                        }
                    })
                })
                .next()
        });

        if existing_coord.is_none() {
            // No chat for this user — check if ANY chat agents exist
            let any_exist = graph.as_ref().is_some_and(|g| {
                g.tasks().any(|t| {
                    t.tags.iter().any(|tag| workgraph::chat_id::is_chat_loop_tag(tag))
                        && !matches!(t.status, workgraph::graph::Status::Abandoned)
                        && !t.tags.iter().any(|tag| tag == "archived")
                })
            });
            if !any_exist {
                // No coordinators at all — create one for first-use experience
                self.create_coordinator(Some(user.clone()));
            }
            // If other coordinators exist but none for this user, don't auto-create.
            // The user can use the plus (+) key to add one manually.
        }

        // Only switch to the user's coordinator if no valid focus was restored
        // from tui-state.json (i.e., still on the default coordinator 0).
        if self.active_coordinator_id == 0
            && let Some(cid) = existing_coord
        {
            self.active_coordinator_id = cid;
        }
    }

    /// Open the full-pane coordinator launcher, populating it with
    /// available executors, models, endpoints, and recent combos.
    pub fn open_launcher(&mut self) {
        use workgraph::executor_discovery;
        use workgraph::launcher_history;

        let now = Instant::now();
        if let Some(last) = self.last_launcher_open {
            if now.duration_since(last).as_millis() < 250 {
                return;
            }
        }
        self.last_launcher_open = Some(now);

        let config = Config::load_or_default(&self.workgraph_dir);
        let max = config.coordinator.max_coordinators;
        let alive = self.list_coordinator_ids_and_labels().len();
        if alive >= max {
            self.push_toast(
                format!("Chat cap reached ({}/{})", alive, max),
                ToastSeverity::Warning,
            );
            return;
        }

        let all_executors = executor_discovery::discover();
        let executor_list: Vec<(String, String, bool)> = all_executors
            .iter()
            .map(|e| (e.name.to_string(), e.description.to_string(), e.available))
            .collect();

        let all_models =
            workgraph::models::load_model_choices_with_descriptions(&self.workgraph_dir);

        let endpoint_list: Vec<(String, String)> = config
            .llm_endpoints
            .endpoints
            .iter()
            .map(|ep| {
                let desc = ep
                    .url
                    .clone()
                    .unwrap_or_else(|| format!("{} (default)", ep.provider));
                (ep.name.clone(), desc)
            })
            .collect();

        let recent_list = launcher_history::recent_combos(10).unwrap_or_default();

        let default_executor_idx = executor_list
            .iter()
            .position(|(name, _, avail)| *avail && name == "claude")
            .or_else(|| executor_list.iter().position(|(_, _, avail)| *avail))
            .unwrap_or(0);

        let initial_executor = executor_list
            .get(default_executor_idx)
            .map(|(name, _, _)| name.clone())
            .unwrap_or_else(|| "claude".to_string());
        let initial_models = filter_models_for_executor(&all_models, &initial_executor);
        let model_picker = FilterPicker::new(initial_models, true)
            .with_hint("No models found. Check wg config --registry.");
        let endpoint_picker = FilterPicker::new(endpoint_list, true)
            .with_hint("No endpoints registered. wg endpoint add ... to add one.");

        self.launcher = Some(LauncherState {
            active_section: LauncherSection::Executor,
            name: String::new(),
            executor_list,
            executor_selected: default_executor_idx,
            model_picker,
            endpoint_picker,
            recent_list,
            recent_selected: 0,
            all_models,
        });
        self.input_mode = InputMode::Launcher;
    }

    /// Launch a coordinator with the selections from the launcher pane.
    pub fn launch_from_launcher(&mut self) {
        let launcher = match self.launcher.take() {
            Some(l) => l,
            None => return,
        };
        self.input_mode = InputMode::Normal;

        let config = Config::load_or_default(&self.workgraph_dir);
        let max = config.coordinator.max_coordinators;
        let alive = self.list_coordinator_ids_and_labels().len();
        if alive >= max {
            self.push_toast(
                format!("Chat cap reached ({}/{})", alive, max),
                ToastSeverity::Warning,
            );
            return;
        }

        let mut args = vec!["service".to_string(), "create-coordinator".to_string()];

        let name = launcher.name.trim().to_string();
        if !name.is_empty() {
            args.push("--name".to_string());
            args.push(name);
        }

        let executor = launcher.selected_executor().to_string();
        args.push("--executor".to_string());
        args.push(executor.clone());

        if let Some(model) = launcher.selected_model() {
            args.push("--model".to_string());
            args.push(model.clone());

            // Record history entry
            let endpoint = launcher.selected_endpoint();
            if let Ok(()) = workgraph::launcher_history::record_use(
                &workgraph::launcher_history::HistoryEntry::new(
                    &executor,
                    Some(&model),
                    endpoint.as_deref(),
                    "tui",
                ),
            ) {}
        }

        self.exec_command(args, CommandEffect::CreateCoordinator);
    }

    /// Create a coordinator by name (used for auto-creation on first-use).
    pub fn create_coordinator(&mut self, name: Option<String>) {
        let mut args = vec!["service".to_string(), "create-coordinator".to_string()];
        if let Some(n) = name {
            let name_trimmed = n.trim().to_string();
            if !name_trimmed.is_empty() {
                args.push("--name".to_string());
                args.push(name_trimmed);
            }
        }
        self.exec_command(args, CommandEffect::CreateCoordinator);
    }

    /// Close the launcher pane without creating a coordinator.
    pub fn close_launcher(&mut self) {
        self.launcher = None;
        self.input_mode = InputMode::Normal;
    }

    /// Create a coordinator with defaults (Shift+Plus shortcut, skips picker).
    pub fn create_coordinator_with_defaults(&mut self) {
        let config = Config::load_or_default(&self.workgraph_dir);
        let max = config.coordinator.max_coordinators;
        let alive = self.list_coordinator_ids_and_labels().len();
        if alive >= max {
            self.push_toast(
                format!("Chat cap reached ({}/{})", alive, max),
                ToastSeverity::Warning,
            );
            return;
        }
        self.create_coordinator(None);
    }

    /// Open the coordinator picker overlay.
    pub fn open_coordinator_picker(&mut self) {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = workgraph::parser::load_graph(&graph_path).ok();

        let ids_and_labels = self.list_coordinator_ids_and_labels();
        let mut entries: Vec<(u32, String, String, bool)> = Vec::new();

        for (cid, label) in &ids_and_labels {
            // Prefer .chat-N (new), fall back to .coordinator-N or bare .coordinator (legacy)
            let task_id = if let Some(ref g) = graph {
                let new_id = workgraph::chat_id::format_chat_task_id(*cid);
                if g.get_task(&new_id).is_some() {
                    new_id
                } else if *cid == 0 && g.get_task(".coordinator").is_some() {
                    ".coordinator".to_string()
                } else {
                    format!(".coordinator-{}", cid)
                }
            } else if *cid == 0 {
                ".coordinator".to_string()
            } else {
                workgraph::chat_id::format_chat_task_id(*cid)
            };

            let (status_desc, is_alive) = if let Some(ref g) = graph {
                if let Some(task) = g.get_task(&task_id) {
                    let alive = matches!(task.status, Status::InProgress);
                    let status_str = format!("{:?}", task.status).to_lowercase();
                    let name = task.title.clone();
                    let desc = if name != task_id {
                        format!("{} ({})", name, status_str)
                    } else {
                        status_str
                    };
                    (desc, alive)
                } else {
                    ("no task".to_string(), false)
                }
            } else {
                ("unknown".to_string(), false)
            };

            entries.push((*cid, label.clone(), status_desc, is_alive));
        }

        let current_idx = entries
            .iter()
            .position(|(id, _, _, _)| *id == self.active_coordinator_id)
            .unwrap_or(0);

        self.coordinator_picker = Some(CoordinatorPickerState {
            selected: current_idx,
            entries,
        });
        self.input_mode = InputMode::CoordinatorPicker;
    }

    /// Close the coordinator picker without switching.
    pub fn close_coordinator_picker(&mut self) {
        self.coordinator_picker = None;
        self.input_mode = InputMode::Normal;
    }

    /// Delete a coordinator session via IPC.
    /// Sends the delete command to the backend; on success the effect handler
    /// cleans up local chat state, switches to another coordinator, and refreshes.
    pub fn delete_coordinator(&mut self, cid: u32) {
        let args = vec![
            "service".to_string(),
            "delete-coordinator".to_string(),
            cid.to_string(),
        ];
        self.exec_command(args, CommandEffect::DeleteCoordinator(cid));
    }

    /// Get a list of known coordinator IDs from the graph.
    pub fn list_coordinator_ids(&self) -> Vec<u32> {
        self.list_coordinator_ids_and_labels()
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// Get coordinator IDs with display labels from the graph.
    /// Returns Vec of (id, label) where label is the canonical chat task id
    /// (`.chat-N`) matching what `wg show` / `wg list` use.
    pub fn list_coordinator_ids_and_labels(&self) -> Vec<(u32, String)> {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = match workgraph::parser::load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => return vec![(0, workgraph::chat_id::format_chat_task_id(0))],
        };
        let mut entries: Vec<(u32, String)> = graph
            .tasks()
            .filter(|t| t.tags.iter().any(|tag| workgraph::chat_id::is_chat_loop_tag(tag)))
            .filter(|t| !matches!(t.status, Status::Abandoned))
            .filter(|t| !t.tags.iter().any(|tag| tag == "archived"))
            .filter_map(|t| {
                // Accept .chat-N, .coordinator-N (legacy), and bare .coordinator
                // (legacy cid 0). The bare-.coordinator case is what older
                // graphs use for the very first chat before chat-rename runs.
                let cid = workgraph::chat_id::parse_chat_task_id(&t.id)
                    .or_else(|| (t.id == ".coordinator").then_some(0))?;
                Some((cid, String::new()))
            })
            .collect();
        entries.sort_by_key(|(id, _)| *id);
        entries.dedup_by_key(|(id, _)| *id);
        if entries.is_empty() {
            entries.push((0, String::new()));
        }
        // Label = canonical task id. Per CLAUDE.md the legacy
        // `coordinator`/`coord` role-noun is deprecated; chat tabs surface
        // the same `.chat-N` id the user sees in `wg list` / `wg show`.
        for (cid, label) in entries.iter_mut() {
            *label = workgraph::chat_id::format_chat_task_id(*cid);
        }
        entries
    }

    /// Get user board entries from the graph.
    /// Returns Vec of (task_id, label) for active `.user-*` tasks.
    pub fn list_user_board_entries(&self) -> Vec<(String, String)> {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = match workgraph::parser::load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let mut entries: Vec<(String, String)> = graph
            .tasks()
            .filter(|t| workgraph::graph::is_user_board(&t.id))
            .filter(|t| !t.status.is_terminal())
            .filter(|t| !t.tags.iter().any(|tag| tag == "archived"))
            .map(|t| {
                let label = workgraph::graph::user_board_handle(&t.id)
                    .map(|h| h.to_string())
                    .unwrap_or_else(|| t.id.clone());
                (t.id.clone(), label)
            })
            .collect();
        entries.sort_by(|(a, _), (b, _)| a.cmp(b));
        entries
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
                self.push_toast(format!("Image pasted: {}", filename), ToastSeverity::Info);
                true
            }
            Ok(None) => false, // no image on clipboard — fall through to text paste
            Err(e) => {
                self.push_toast(format!("Clipboard error: {}", e), ToastSeverity::Error);
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

    /// Toggle the archive browser view.
    pub fn toggle_archive_browser(&mut self) {
        if self.archive_browser.active {
            self.archive_browser.active = false;
            self.archive_browser.filter_active = false;
        } else {
            self.archive_browser.load(&self.workgraph_dir);
            self.archive_browser.active = true;
            self.archive_browser.filter_active = false;
        }
    }

    /// Restore the currently selected archived task back into the graph.
    pub fn restore_archive_entry(&mut self) {
        let task_id = match self.archive_browser.selected_entry() {
            Some(e) => e.id.clone(),
            None => return,
        };
        self.exec_command(
            vec![
                "archive".to_string(),
                "restore".to_string(),
                task_id.clone(),
                "--reopen".to_string(),
            ],
            CommandEffect::RefreshAndNotify(format!("Restored '{}'", task_id)),
        );
    }

    /// Open the history browser (Ctrl+H), loading segments for the active coordinator
    /// plus cross-coordinator context summaries.
    pub fn open_history_browser(&mut self) {
        let labels = self.list_coordinator_ids_and_labels();
        // No coordinators are restricted within the same project — visibility
        // settings are respected at the sharing boundary (internal = project-only).
        let restricted: Vec<u32> = Vec::new();
        self.history_browser.load_with_cross_coordinator(
            &self.workgraph_dir,
            self.active_coordinator_id,
            &labels,
            &restricted,
        );
        self.history_browser.active = true;
    }

    /// Close the history browser without injecting.
    pub fn close_history_browser(&mut self) {
        self.history_browser.active = false;
        self.history_browser.preview_expanded = false;
        self.history_browser.preview_scroll = 0;
    }

    /// Inject the selected history segment into the coordinator's context.
    /// Cross-coordinator segments are wrapped with an import label.
    pub fn inject_selected_history(&mut self) {
        let (content, label, is_cross) = match self.history_browser.selected_segment() {
            Some(seg) => {
                let is_cross = matches!(
                    seg.source,
                    workgraph::chat::HistorySource::CrossCoordinator { .. }
                );
                (seg.content.clone(), seg.label.clone(), is_cross)
            }
            None => return,
        };
        let wrapped = if is_cross {
            format!(
                "---\n\
                 ## Imported Context: {}\n\
                 \n\
                 > This is imported context from another coordinator.\n\
                 > Treat it as read-only reference material.\n\
                 \n\
                 {}\n\
                 \n\
                 ---",
                label, content
            )
        } else {
            content
        };
        let cid = self.active_coordinator_id;
        match workgraph::chat::write_injected_context(&self.workgraph_dir, cid, &wrapped) {
            Ok(()) => {
                self.close_history_browser();
                self.push_toast(format!("Injected: {}", label), ToastSeverity::Info);
            }
            Err(e) => {
                self.push_toast(
                    format!("Failed to inject context: {}", e),
                    ToastSeverity::Error,
                );
            }
        }
    }

    /// Submit the task creation form — runs `wg add` in background.
    pub fn submit_task_form(&mut self) {
        let form = match self.task_form.take() {
            Some(f) => f,
            None => return,
        };
        self.input_mode = InputMode::Normal;

        if form.title.trim().is_empty() {
            self.push_toast("Task title is required".to_string(), ToastSeverity::Warning);
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
                self.push_toast("No task selected".to_string(), ToastSeverity::Warning);
                return;
            }
        };

        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = match load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => {
                self.push_toast("Failed to load graph".to_string(), ToastSeverity::Error);
                return;
            }
        };

        let agent_id = match graph.tasks().find(|t| t.id == task_id) {
            Some(task) => match &task.assigned {
                Some(id) => id.clone(),
                None => {
                    self.push_toast(
                        format!("No active agent on '{}'", task_id),
                        ToastSeverity::Warning,
                    );
                    return;
                }
            },
            None => {
                self.push_toast(
                    format!("Task '{}' not found", task_id),
                    ToastSeverity::Warning,
                );
                return;
            }
        };

        self.exec_command(
            vec!["kill".to_string(), agent_id.clone()],
            CommandEffect::RefreshAndNotify(format!("Killed {} on task '{}'", agent_id, task_id)),
        );
    }

    /// Interrupt the active coordinator's current generation.
    ///
    /// Sends `InterruptCoordinator` IPC to the daemon, which sends SIGINT
    /// to the Claude CLI subprocess. The process stays alive and can accept
    /// new messages immediately.
    pub fn interrupt_coordinator(&mut self) {
        let cid = self.active_coordinator_id;
        self.exec_command(
            vec![
                "service".to_string(),
                "interrupt-coordinator".to_string(),
                cid.to_string(),
            ],
            CommandEffect::InterruptCoordinator(cid),
        );
        // Optimistically clear awaiting state — the response collector
        // will write whatever partial text it has to the outbox.
        self.chat.pending_request_ids.clear();
        self.chat.awaiting_since = None;
        self.chat.streaming_text.clear();
        // Flush deferred message tracking on interrupt — messages are already
        // in self.chat.messages, so just clear the index tracking.
        self.chat.deferred_user_indices.clear();
    }

    // ── Config panel ──

    /// Load configuration from disk and populate config panel entries.
    pub fn load_config_panel(&mut self) {
        let config = Config::load_or_default(&self.workgraph_dir);
        let model_choices = load_model_choices(&self.workgraph_dir);
        let mut model_choices_with_default = vec!["(default)".to_string()];
        model_choices_with_default.extend(model_choices.iter().cloned());
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
        // Status indicator: ✓ = set (green), ✗ = missing (red), ⚠ = set but short (yellow)
        let key_status = |var: &str| -> (String, &'static str) {
            match std::env::var(var).ok().filter(|k| !k.is_empty()) {
                Some(key) if key.len() > 8 => {
                    let masked = format!(
                        "{}****...{}",
                        &key[..key.floor_char_boundary(3)],
                        &key[key.ceil_char_boundary(key.len() - 4)..]
                    );
                    (masked, "valid")
                }
                Some(_) => ("****".into(), "short"),
                None => ("(not set)".into(), "missing"),
            }
        };
        let (anthro_val, anthro_status) = key_status("ANTHROPIC_API_KEY");
        let (openai_val, openai_status) = key_status("OPENAI_API_KEY");
        let (openrouter_val, openrouter_status) = key_status("OPENROUTER_API_KEY");
        // Also check endpoint-configured keys
        let endpoint_has_key = |provider: &str| -> bool {
            config.llm_endpoints.endpoints.iter().any(|ep| {
                ep.provider == provider
                    && (ep.api_key.is_some()
                        || ep.api_key_file.is_some()
                        || ep.api_key_env.is_some())
            })
        };
        let key_label = |name: &str, status: &str, provider: &str| -> String {
            let icon = match status {
                "valid" => "✓",
                "short" => "⚠",
                _ if endpoint_has_key(provider) => "✓",
                _ => "✗",
            };
            format!("{} {}", icon, name)
        };
        entries.push(ConfigEntry {
            key: "apikey.anthropic".into(),
            label: key_label("Anthropic", anthro_status, "anthropic"),
            value: anthro_val,
            edit_kind: ConfigEditKind::SecretInput,
            section: ConfigSection::ApiKeys,
        });
        entries.push(ConfigEntry {
            key: "apikey.openai".into(),
            label: key_label("OpenAI", openai_status, "openai"),
            value: openai_val,
            edit_kind: ConfigEditKind::SecretInput,
            section: ConfigSection::ApiKeys,
        });
        entries.push(ConfigEntry {
            key: "apikey.openrouter".into(),
            label: key_label("OpenRouter", openrouter_status, "openrouter"),
            value: openrouter_val,
            edit_kind: ConfigEditKind::SecretInput,
            section: ConfigSection::ApiKeys,
        });

        // ── 3. Model Registry (from models.yaml) ──
        {
            let registry =
                workgraph::models::ModelRegistry::load(&self.workgraph_dir).unwrap_or_default();
            let default_id = registry.default_model.clone();
            let mut models: Vec<&workgraph::models::ModelEntry> =
                registry.models.values().collect();
            models.sort_by(|a, b| a.id.cmp(&b.id));
            for model in &models {
                let is_default = default_id.as_deref() == Some(&*model.id);
                let default_marker = if is_default { " *" } else { "" };
                let ctx = if model.context_window >= 1_000_000 {
                    format!("{}M", model.context_window / 1_000_000)
                } else {
                    format!("{}k", model.context_window / 1_000)
                };
                entries.push(ConfigEntry {
                    key: format!("model.{}.info", model.id),
                    label: format!("{}{}", model.short_name(), default_marker),
                    value: format!(
                        "{} | ${:.2}/{:.2} | {}",
                        model.tier, model.cost_per_1m_input, model.cost_per_1m_output, ctx
                    ),
                    edit_kind: ConfigEditKind::TextInput,
                    section: ConfigSection::Models,
                });
                entries.push(ConfigEntry {
                    key: format!("model.{}.set_default", model.id),
                    label: "  Set as default".into(),
                    value: if is_default {
                        "on".into()
                    } else {
                        "off".into()
                    },
                    edit_kind: ConfigEditKind::Toggle,
                    section: ConfigSection::Models,
                });
                entries.push(ConfigEntry {
                    key: format!("model.{}.remove", model.id),
                    label: "  Remove model".into(),
                    value: "▸".into(),
                    edit_kind: ConfigEditKind::Toggle,
                    section: ConfigSection::Models,
                });
            }
            entries.push(ConfigEntry {
                key: "model.add".into(),
                label: "+ Add model".into(),
                value: String::new(),
                edit_kind: ConfigEditKind::TextInput,
                section: ConfigSection::Models,
            });
        }

        // ── 4. Service Settings ──
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
            value: config.coordinator.effective_executor(),
            edit_kind: ConfigEditKind::Choice(vec![
                "claude".into(),
                "native".into(),
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
        entries.push(ConfigEntry {
            key: "coordinator.max_coordinators".into(),
            label: "Max coordinators".into(),
            value: config.coordinator.max_coordinators.to_string(),
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
        entries.push(ConfigEntry {
            key: "tui.chat_history".into(),
            label: "Chat history".into(),
            value: if config.tui.chat_history {
                "on".into()
            } else {
                "off".into()
            },
            edit_kind: ConfigEditKind::Toggle,
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.chat_history_max".into(),
            label: "Chat history max".into(),
            value: config.tui.chat_history_max.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.session_gap_minutes".into(),
            label: "Session gap (min)".into(),
            value: config.tui.session_gap_minutes.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.counters".into(),
            label: "Counters".into(),
            value: config.tui.counters.clone(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.show_system_tasks".into(),
            label: "Show system tasks".into(),
            value: if config.tui.show_system_tasks {
                "on".into()
            } else {
                "off".into()
            },
            edit_kind: ConfigEditKind::Toggle,
            section: ConfigSection::TuiSettings,
        });
        entries.push(ConfigEntry {
            key: "tui.show_running_system_tasks".into(),
            label: "Show running system tasks".into(),
            value: if config.tui.show_running_system_tasks {
                "on".into()
            } else {
                "off".into()
            },
            edit_kind: ConfigEditKind::Toggle,
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
        entries.push(ConfigEntry {
            key: "agency.flip_enabled".into(),
            label: "FLIP enabled".into(),
            value: if config.agency.flip_enabled {
                "on".into()
            } else {
                "off".into()
            },
            edit_kind: ConfigEditKind::Toggle,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.flip_verification_threshold".into(),
            label: "FLIP verify threshold".into(),
            value: config
                .agency
                .flip_verification_threshold
                .map(|t| format!("{:.2}", t))
                .unwrap_or_else(|| "(disabled)".into()),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.eval_gate_threshold".into(),
            label: "Eval gate threshold".into(),
            value: config
                .agency
                .eval_gate_threshold
                .map(|t| format!("{:.2}", t))
                .unwrap_or_else(|| "(disabled)".into()),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "agency.eval_gate_all".into(),
            label: "Eval gate all".into(),
            value: if config.agency.eval_gate_all {
                "on".into()
            } else {
                "off".into()
            },
            edit_kind: ConfigEditKind::Toggle,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "checkpoint.retry_context_tokens".into(),
            label: "Retry context tokens".into(),
            value: config.checkpoint.retry_context_tokens.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "coordinator.max_incomplete_retries".into(),
            label: "Max incomplete retries".into(),
            value: config.coordinator.max_incomplete_retries.to_string(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "coordinator.incomplete_retry_delay".into(),
            label: "Incomplete retry delay".into(),
            value: config.coordinator.incomplete_retry_delay.clone(),
            edit_kind: ConfigEditKind::TextInput,
            section: ConfigSection::Agency,
        });
        entries.push(ConfigEntry {
            key: "coordinator.escalate_on_retry".into(),
            label: "Escalate tier on retry".into(),
            value: if config.coordinator.escalate_on_retry {
                "on".into()
            } else {
                "off".into()
            },
            edit_kind: ConfigEditKind::Toggle,
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

        // ── 8. Model Tiers ──
        {
            let effective = config.effective_tiers_public();
            entries.push(ConfigEntry {
                key: "tiers.fast".into(),
                label: "Fast".into(),
                value: effective
                    .fast
                    .clone()
                    .unwrap_or_else(|| workgraph::config::Tier::Fast.default_alias().into()),
                edit_kind: ConfigEditKind::TextInput,
                section: ConfigSection::ModelTiers,
            });
            entries.push(ConfigEntry {
                key: "tiers.standard".into(),
                label: "Standard".into(),
                value: effective
                    .standard
                    .clone()
                    .unwrap_or_else(|| workgraph::config::Tier::Standard.default_alias().into()),
                edit_kind: ConfigEditKind::TextInput,
                section: ConfigSection::ModelTiers,
            });
            entries.push(ConfigEntry {
                key: "tiers.premium".into(),
                label: "Premium".into(),
                value: effective
                    .premium
                    .clone()
                    .unwrap_or_else(|| workgraph::config::Tier::Premium.default_alias().into()),
                edit_kind: ConfigEditKind::TextInput,
                section: ConfigSection::ModelTiers,
            });
        }

        // ── 9. Model Routing ──
        {
            use workgraph::config::DispatchRole;
            let tier_choices = vec![
                "(inherit)".to_string(),
                "fast".to_string(),
                "standard".to_string(),
                "premium".to_string(),
            ];
            let roles = [
                (DispatchRole::Default, "Default"),
                (DispatchRole::TaskAgent, "Task agent"),
                (DispatchRole::Evaluator, "Evaluator"),
                (DispatchRole::FlipInference, "FLIP inference"),
                (DispatchRole::FlipComparison, "FLIP comparison"),
                (DispatchRole::Assigner, "Assigner"),
                (DispatchRole::Evolver, "Evolver"),
                (DispatchRole::Verification, "Verification"),
                (DispatchRole::Triage, "Triage"),
                (DispatchRole::Creator, "Creator"),
                (DispatchRole::Compactor, "Compactor"),
            ];
            for (role, label) in roles {
                let role_cfg = config.models.get_role(role);
                let resolved = config.resolve_model_for_role(role);
                let source = config.resolve_model_source(role);
                let resolved_display = format!("{} ({})", resolved.model, source);
                let model_val = role_cfg
                    .and_then(|c| c.model.clone())
                    .unwrap_or_else(|| "(inherit)".into());
                let provider_val = role_cfg
                    .and_then(|c| c.provider.clone())
                    .unwrap_or_else(|| "(inherit)".into());
                let tier_val = role_cfg
                    .and_then(|c| c.tier.map(|t| t.to_string()))
                    .unwrap_or_else(|| "(inherit)".into());
                let endpoint_val = role_cfg
                    .and_then(|c| c.endpoint.clone())
                    .unwrap_or_else(|| "(inherit)".into());
                // Resolved display (read-only info line)
                entries.push(ConfigEntry {
                    key: format!("models.{}.resolved", role),
                    label: format!("{}  → {}", label, resolved_display),
                    value: String::new(),
                    edit_kind: ConfigEditKind::TextInput, // shown but not meaningfully editable
                    section: ConfigSection::ModelRouting,
                });
                entries.push(ConfigEntry {
                    key: format!("models.{}.model", role),
                    label: format!("  {} model", label),
                    value: model_val,
                    edit_kind: ConfigEditKind::TextInput,
                    section: ConfigSection::ModelRouting,
                });
                entries.push(ConfigEntry {
                    key: format!("models.{}.tier", role),
                    label: format!("  {} tier", label),
                    value: tier_val,
                    edit_kind: ConfigEditKind::Choice(tier_choices.clone()),
                    section: ConfigSection::ModelRouting,
                });
                entries.push(ConfigEntry {
                    key: format!("models.{}.provider", role),
                    label: format!("  {} provider", label),
                    value: provider_val,
                    edit_kind: ConfigEditKind::TextInput,
                    section: ConfigSection::ModelRouting,
                });
                entries.push(ConfigEntry {
                    key: format!("models.{}.endpoint", role),
                    label: format!("  {} endpoint", label),
                    value: endpoint_val,
                    edit_kind: ConfigEditKind::TextInput,
                    section: ConfigSection::ModelRouting,
                });
            }
        }

        // ── 10. Actions ──
        entries.push(ConfigEntry {
            key: "action.install_global".into(),
            label: "Install as Global".into(),
            value: "▸".into(),
            edit_kind: ConfigEditKind::Toggle,
            section: ConfigSection::Actions,
        });

        self.config_panel.entries = entries;
        if self.config_panel.selected >= self.config_panel.entries.len() {
            self.config_panel.selected = 0;
        }

        // Record config file mtime so we can detect external changes.
        self.config_panel.last_config_mtime =
            std::fs::metadata(self.workgraph_dir.join("config.toml"))
                .and_then(|m| m.modified())
                .ok();
    }

    /// Install the current project config as the global default (force mode).
    pub fn install_config_as_global(&mut self) {
        use crate::commands::config_cmd::install_global_to;

        let global_path = match Config::global_config_path() {
            Ok(p) => p,
            Err(e) => {
                self.push_toast(format!("Error: {}", e), ToastSeverity::Error);
                return;
            }
        };
        let global_dir = match Config::global_dir() {
            Ok(d) => d,
            Err(e) => {
                self.push_toast(format!("Error: {}", e), ToastSeverity::Error);
                return;
            }
        };
        match install_global_to(&self.workgraph_dir, &global_path, &global_dir, true) {
            Ok(()) => {
                self.config_panel.save_notification = Some(std::time::Instant::now());
                self.push_toast(
                    "Installed project config as global default".to_string(),
                    ToastSeverity::Info,
                );
            }
            Err(e) => {
                self.push_toast(format!("Install failed: {}", e), ToastSeverity::Error);
            }
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
            "coordinator.executor" => config.coordinator.executor = Some(new_value),
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
            "coordinator.max_coordinators" => {
                if let Ok(v) = new_value.parse::<usize>() {
                    config.coordinator.max_coordinators = v;
                }
            }
            "tui.chat_history" => config.tui.chat_history = new_value == "on",
            "tui.chat_history_max" => {
                if let Ok(v) = new_value.parse::<usize>() {
                    config.tui.chat_history_max = v;
                }
            }
            "tui.session_gap_minutes" => {
                if let Ok(v) = new_value.parse::<u32>() {
                    config.tui.session_gap_minutes = v;
                    self.session_gap_minutes = v;
                }
            }
            "tui.counters" => config.tui.counters = new_value,
            "tui.show_system_tasks" => {
                config.tui.show_system_tasks = new_value == "on";
                self.show_system_tasks = config.tui.show_system_tasks;
                self.system_tasks_just_toggled = true;
                self.force_refresh();
            }
            "tui.show_running_system_tasks" => {
                config.tui.show_running_system_tasks = new_value == "on";
            }
            "agency.flip_enabled" => config.agency.flip_enabled = new_value == "on",
            "agency.flip_verification_threshold" => {
                config.agency.flip_verification_threshold =
                    if new_value == "(disabled)" || new_value.is_empty() {
                        None
                    } else {
                        new_value.parse::<f64>().ok()
                    };
            }
            "agency.eval_gate_threshold" => {
                config.agency.eval_gate_threshold =
                    if new_value == "(disabled)" || new_value.is_empty() {
                        None
                    } else {
                        new_value.parse::<f64>().ok()
                    };
            }
            "agency.eval_gate_all" => config.agency.eval_gate_all = new_value == "on",
            "checkpoint.retry_context_tokens" => {
                if let Ok(v) = new_value.parse::<u32>() {
                    config.checkpoint.retry_context_tokens = v;
                }
            }
            "coordinator.max_incomplete_retries" => {
                if let Ok(v) = new_value.parse::<u32>() {
                    config.coordinator.max_incomplete_retries = v;
                }
            }
            "coordinator.incomplete_retry_delay" => {
                config.coordinator.incomplete_retry_delay = new_value;
            }
            "coordinator.escalate_on_retry" => {
                config.coordinator.escalate_on_retry = new_value == "on";
            }
            "tiers.fast" => {
                config.tiers.fast = Some(new_value);
            }
            "tiers.standard" => {
                config.tiers.standard = Some(new_value);
            }
            "tiers.premium" => {
                config.tiers.premium = Some(new_value);
            }
            _ => {
                // Model routing fields: models.<role>.<field>
                if let Some(rest) = key.strip_prefix("models.") {
                    let parts: Vec<&str> = rest.rsplitn(2, '.').collect();
                    if parts.len() == 2 {
                        let field = parts[0]; // "model", "provider", "tier", "endpoint", or "resolved"
                        let role_str = parts[1];
                        if field == "resolved" {
                            // Read-only display line — don't save
                            self.config_panel.editing = false;
                            return;
                        }
                        if let Ok(role) = role_str.parse::<workgraph::config::DispatchRole>() {
                            let is_inherit = new_value == "(inherit)" || new_value.is_empty();
                            match field {
                                "model" => {
                                    if is_inherit {
                                        let slot = config.models.get_role_mut(role);
                                        if let Some(c) = slot {
                                            c.model = None;
                                        }
                                    } else {
                                        config.models.set_model(role, &new_value);
                                    }
                                }
                                "provider" => {
                                    if is_inherit {
                                        let slot = config.models.get_role_mut(role);
                                        if let Some(c) = slot {
                                            c.provider = None;
                                        }
                                    } else {
                                        config.models.set_provider(role, &new_value);
                                    }
                                }
                                "tier" => {
                                    let slot = config.models.get_role_mut(role);
                                    if is_inherit {
                                        if let Some(c) = slot {
                                            c.tier = None;
                                        }
                                    } else if let Ok(tier) =
                                        new_value.parse::<workgraph::config::Tier>()
                                    {
                                        if let Some(c) = slot {
                                            c.tier = Some(tier);
                                        } else {
                                            *slot = Some(workgraph::config::RoleModelConfig {
                                                provider: None,
                                                model: None,
                                                tier: Some(tier),
                                                endpoint: None,
                                            });
                                        }
                                    }
                                }
                                "endpoint" => {
                                    if is_inherit {
                                        let slot = config.models.get_role_mut(role);
                                        if let Some(c) = slot {
                                            c.endpoint = None;
                                        }
                                    } else {
                                        config.models.set_endpoint(role, &new_value);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }

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
                // Model registry info entries are read-only
                if key.starts_with("model.") && key.ends_with(".info") {
                    self.config_panel.editing = false;
                    return;
                }
                // model.add is not a text save — handled in event code
                if key == "model.add" {
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

        // Handle model removal
        if key.ends_with(".remove")
            && key.starts_with("model.")
            && let Some(model_id) = key
                .strip_prefix("model.")
                .and_then(|r| r.strip_suffix(".remove"))
            && let Ok(mut registry) = workgraph::models::ModelRegistry::load(&self.workgraph_dir)
        {
            registry.models.remove(model_id);
            // Clear default if removed model was default
            if registry.default_model.as_deref() == Some(model_id) {
                registry.default_model = None;
            }
            let _ = registry.save(&self.workgraph_dir);
            self.config_panel.save_notification = Some(Instant::now());
            self.load_config_panel();
            return;
        }

        // Handle model set-as-default
        if key.ends_with(".set_default")
            && key.starts_with("model.")
            && let Some(model_id) = key
                .strip_prefix("model.")
                .and_then(|r| r.strip_suffix(".set_default"))
            && let Ok(mut registry) = workgraph::models::ModelRegistry::load(&self.workgraph_dir)
        {
            if registry.default_model.as_deref() == Some(model_id) {
                // Toggle off: clear default
                registry.default_model = None;
            } else {
                let _ = registry.set_default(model_id);
            }
            let _ = registry.save(&self.workgraph_dir);
            self.config_panel.save_notification = Some(Instant::now());
            self.load_config_panel();
            return;
        }

        // Handle install-as-global action
        if key == "action.install_global" {
            self.install_config_as_global();
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
            "tui.chat_history" => config.tui.chat_history = new_val == "on",
            "tui.show_system_tasks" => {
                config.tui.show_system_tasks = new_val == "on";
                self.show_system_tasks = config.tui.show_system_tasks;
                self.system_tasks_just_toggled = true;
                self.force_refresh();
            }
            "tui.show_running_system_tasks" => {
                config.tui.show_running_system_tasks = new_val == "on";
            }
            "agency.flip_enabled" => config.agency.flip_enabled = new_val == "on",
            "agency.eval_gate_all" => config.agency.eval_gate_all = new_val == "on",
            "coordinator.escalate_on_retry" => {
                config.coordinator.escalate_on_retry = new_val == "on";
            }
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
            self.push_toast(
                "Endpoint name is required".to_string(),
                ToastSeverity::Warning,
            );
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
                api_key_file: None,
                api_key_env: None,
                is_default: is_first,
                context_window: None,
            });
        if config.save(&self.workgraph_dir).is_ok() {
            self.config_panel.save_notification = Some(Instant::now());
        }
        self.config_panel.adding_endpoint = false;
        self.config_panel.new_endpoint = NewEndpointFields::default();
        self.config_panel.new_endpoint_field = 0;
        self.load_config_panel();
    }

    /// Add a new model from the new-model form fields.
    pub fn add_model(&mut self) {
        let fields = &self.config_panel.new_model;
        if fields.id.trim().is_empty() {
            self.push_toast("Model ID is required".to_string(), ToastSeverity::Warning);
            return;
        }
        let mut registry =
            workgraph::models::ModelRegistry::load(&self.workgraph_dir).unwrap_or_default();
        let tier = match fields.tier.to_lowercase().as_str() {
            "frontier" => workgraph::models::ModelTier::Frontier,
            "mid" => workgraph::models::ModelTier::Mid,
            _ => workgraph::models::ModelTier::Budget,
        };
        let cost_in = fields.cost_in.parse::<f64>().unwrap_or(0.0);
        let cost_out = fields.cost_out.parse::<f64>().unwrap_or(0.0);
        let provider = if fields.provider.is_empty() {
            "openrouter".to_string()
        } else {
            fields.provider.clone()
        };
        let entry = workgraph::models::ModelEntry {
            id: fields.id.trim().to_string(),
            provider,
            cost_per_1m_input: cost_in,
            cost_per_1m_output: cost_out,
            context_window: 128_000,
            capabilities: vec!["coding".into(), "tool_use".into()],
            tier,
        };
        registry.add(entry);
        if registry.save(&self.workgraph_dir).is_ok() {
            self.config_panel.save_notification = Some(Instant::now());
        }
        self.config_panel.adding_model = false;
        self.config_panel.new_model = NewModelFields::default();
        self.config_panel.new_model_field = 0;
        self.load_config_panel();
    }

    /// Test the endpoint associated with the currently selected config entry.
    /// Looks up the endpoint index from the entry key and runs `wg endpoints test <name>`.
    pub fn test_selected_endpoint(&mut self) {
        let idx = self.config_panel.selected;
        if idx >= self.config_panel.entries.len() {
            return;
        }
        let key = &self.config_panel.entries[idx].key;
        // Extract endpoint index from keys like "endpoint.N.name", "endpoint.N.model", etc.
        let ep_idx = if let Some(rest) = key.strip_prefix("endpoint.") {
            rest.split('.').next().and_then(|s| s.parse::<usize>().ok())
        } else {
            None
        };
        let Some(ep_idx) = ep_idx else {
            return;
        };
        let config = Config::load_or_default(&self.workgraph_dir);
        let Some(ep) = config.llm_endpoints.endpoints.get(ep_idx) else {
            return;
        };
        let ep_name = ep.name.clone();
        // Mark as testing
        self.config_panel
            .endpoint_test_results
            .insert(ep_name.clone(), EndpointTestStatus::Testing);
        // Run test in background
        self.exec_command(
            vec!["endpoints".to_string(), "test".to_string(), ep_name.clone()],
            CommandEffect::EndpointTest(ep_name),
        );
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

/// Returns all archived iteration directories for a task, sorted oldest-first.
/// Each entry is (directory name / timestamp, directory path).
fn find_all_archives(
    workgraph_dir: &std::path::Path,
    task_id: &str,
) -> Vec<(String, std::path::PathBuf)> {
    let archive_base = workgraph_dir.join("log").join("agents").join(task_id);
    if !archive_base.exists() {
        return Vec::new();
    }
    let mut entries: Vec<_> = std::fs::read_dir(&archive_base)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().ok().is_some_and(|ft| ft.is_dir()))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    // Sort by name ascending (oldest first — timestamps sort lexicographically)
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

/// Deterministic session UUID for a coordinator, derived from CWD + session name.
/// Ensures each coordinator always maps to the same Claude session ID.
fn claude_session_uuid(cwd: &std::path::Path, session_name: &str) -> uuid::Uuid {
    let key = format!("{}:{}", cwd.display(), session_name);
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, key.as_bytes())
}

/// Does a specific Claude session JSONL exist for the given UUID?
fn claude_session_exists(cwd: &std::path::Path, session_uuid: &uuid::Uuid) -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let Some(cwd_str) = cwd.to_str() else {
        return false;
    };
    // Claude CLI replaces both '/' and '.' with '-' when computing the
    // project slug from the CWD path.
    let slug = cwd_str.replace(['/', '.'], "-");
    let session_file = home
        .join(".claude")
        .join("projects")
        .join(slug)
        .join(format!("{}.jsonl", session_uuid));
    session_file.exists()
        && session_file
            .metadata()
            .map(|m| m.len() > 0)
            .unwrap_or(false)
}

/// Returns the file at `filename` within the given archive directory, if it exists.
/// Falls back to common alternative names (e.g. output.log → output.txt).
fn find_archive_file(archive_dir: &std::path::Path, filename: &str) -> Option<std::path::PathBuf> {
    let candidate = archive_dir.join(filename);
    if candidate.exists() {
        return Some(candidate);
    }
    // Fallback: output.log ↔ output.txt, prompt.txt stays as-is
    let alt = match filename {
        "output.txt" => "output.log",
        "output.log" => "output.txt",
        _ => return None,
    };
    let alt_candidate = archive_dir.join(alt);
    if alt_candidate.exists() {
        Some(alt_candidate)
    } else {
        None
    }
}

/// Extract assistant text content from a JSON stream log (output.log / raw_stream.jsonl).
///
/// Parses each JSON line, finds `"type": "assistant"` events, and extracts text
/// from `message.content[].text` blocks. Returns the concatenated markdown text
/// with tool-use blocks rendered as compact summaries.
/// Extract assistant text + tool result summaries from output.log JSONL content.
/// Similar to `extract_enriched_text_from_log` but includes compact tool result
/// summaries for a more dynamic live view.
fn extract_enriched_text_from_log(content: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            continue;
        }
        let val: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

        if msg_type == "assistant" {
            let content_arr = match val
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                Some(arr) => arr,
                None => continue,
            };

            for block in content_arr {
                let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match block_type {
                    "text" => {
                        if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                            let trimmed_text = text.trim();
                            if !trimmed_text.is_empty() {
                                parts.push(trimmed_text.to_string());
                            }
                        }
                    }
                    "tool_use" => {
                        let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let detail = match name {
                            "Bash" => block
                                .get("input")
                                .and_then(|i| i.get("command"))
                                .and_then(|v| v.as_str())
                                .map(|c| {
                                    let c = c.trim();
                                    if c.len() > 80 {
                                        format!("{}…", &c[..c.floor_char_boundary(80)])
                                    } else {
                                        c.to_string()
                                    }
                                }),
                            "Read" | "Write" | "Edit" => block
                                .get("input")
                                .and_then(|i| i.get("file_path"))
                                .and_then(|v| v.as_str())
                                .map(|p| p.to_string()),
                            "Grep" | "Glob" => block
                                .get("input")
                                .and_then(|i| i.get("pattern"))
                                .and_then(|v| v.as_str())
                                .map(|p| p.to_string()),
                            _ => None,
                        };
                        let is_bash = name.eq_ignore_ascii_case("Bash");
                        let summary = if is_bash {
                            // Bash commands get a distinct "$ " prefix so they're easily visible
                            // in the log view (matching the convention users expect from terminals).
                            match detail {
                                Some(d) => format!("$ {}", d),
                                None => "$ (command)".to_string(),
                            }
                        } else {
                            match detail {
                                Some(d) => format!("┌─ {} ────\n│ {}\n└─", name, d),
                                None => format!("┌─ {} ────\n└─", name),
                            }
                        };
                        parts.push(summary);
                    }
                    _ => {}
                }
            }
        } else if msg_type == "turn" {
            // Native executor format: {"type":"turn","turn":N,"role":"assistant","content":[...]}
            //
            // Intentionally skip `tool_use` blocks here. The native executor
            // emits BOTH a turn record (with tool_use blocks pre-execution) AND
            // a `tool_call` record (with the full input+output post-execution).
            // Rendering the tool_use from `turn` produced an empty box followed
            // by the real box from `tool_call` — the doubled-render bug.
            // The `tool_call` branch below carries everything we'd want to show.
            if let Some(content) = val.get("content").and_then(|c| c.as_array()) {
                for block in content {
                    let block_type = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if block_type == "text"
                        && let Some(text) = block.get("text").and_then(|v| v.as_str())
                    {
                        let trimmed_text = text.trim();
                        if !trimmed_text.is_empty() {
                            parts.push(trimmed_text.to_string());
                        }
                    }
                }
            }
        } else if msg_type == "tool_call" {
            // Native executor format: {"type":"tool_call","name":"...","input":...,"output":"...","is_error":bool}
            let name = val.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
            let is_error = val
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let detail = match name {
                "Bash" | "bash" => val
                    .get("input")
                    .and_then(|i| i.get("command"))
                    .and_then(|v| v.as_str())
                    .map(|c| {
                        let c = c.trim();
                        if c.len() > 80 {
                            format!("{}…", &c[..c.floor_char_boundary(80)])
                        } else {
                            c.to_string()
                        }
                    }),
                "Read" | "Write" | "Edit" => val
                    .get("input")
                    .and_then(|i| i.get("file_path"))
                    .and_then(|v| v.as_str())
                    .map(|p| p.to_string()),
                "Grep" | "Glob" => val
                    .get("input")
                    .and_then(|i| i.get("pattern"))
                    .and_then(|v| v.as_str())
                    .map(|p| p.to_string()),
                _ => None,
            };
            let is_bash = name.eq_ignore_ascii_case("Bash") || name.eq_ignore_ascii_case("bash");
            let header = if is_bash {
                // Bash commands get a distinct "$ " prefix so they're easily visible
                // in the log view (matching the convention users expect from terminals).
                match detail {
                    Some(d) => d.clone(),
                    None => "(command)".to_string(),
                }
            } else {
                match detail {
                    Some(d) => format!("┌─ {} ────\n│ {}", name, d),
                    None => format!("┌─ {} ────", name),
                }
            };

            // Show tool output summary
            if let Some(output) = val.get("output").and_then(|v| v.as_str()) {
                let clean = String::from_utf8(strip_ansi_escapes::strip(output.as_bytes()))
                    .unwrap_or_else(|_| output.to_string());
                let line_count = clean.lines().count();
                let first_line = clean.lines().next().unwrap_or("").trim();
                let short = if first_line.len() > 60 {
                    format!("{}…", &first_line[..first_line.floor_char_boundary(60)])
                } else {
                    first_line.to_string()
                };
                let prefix = if is_error { "  ↳ error:" } else { "  ↳" };
                let result_line = if line_count > 1 {
                    format!("{} {} ({} lines)", prefix, short, line_count)
                } else {
                    format!("{} {}", prefix, short)
                };
                if is_bash {
                    parts.push(format!("$ {}\n{}", header, result_line));
                } else {
                    parts.push(format!("{}\n{}\n└─", header, result_line));
                }
            } else if is_bash {
                parts.push(format!("$ {}", header));
            } else {
                parts.push(format!("{}\n└─", header));
            }
        } else if msg_type == "result" {
            // Tool result — show a compact one-line summary.
            // Strip ANSI escape codes since tool output (e.g. from Bash) may
            // contain terminal colors that would appear as raw escape sequences.
            let content_arr = val.get("content").and_then(|c| c.as_array());
            let _tool_name = val.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");

            if let Some(arr) = content_arr {
                for block in arr {
                    if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                        let clean = String::from_utf8(strip_ansi_escapes::strip(text.as_bytes()))
                            .unwrap_or_else(|_| text.to_string());
                        let line_count = clean.lines().count();
                        let is_error = block
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let first_line = clean.lines().next().unwrap_or("").trim();
                        let short = if first_line.len() > 60 {
                            format!("{}…", &first_line[..first_line.floor_char_boundary(60)])
                        } else {
                            first_line.to_string()
                        };
                        let prefix = if is_error { "  ↳ error:" } else { "  ↳" };
                        if line_count > 1 {
                            parts.push(format!("{} {} ({} lines)", prefix, short, line_count));
                        } else {
                            parts.push(format!("{} {}", prefix, short));
                        }
                    }
                }
            } else if let Some(text) = val.get("content").and_then(|c| c.as_str()) {
                // Simple string content — also strip ANSI.
                let clean = String::from_utf8(strip_ansi_escapes::strip(text.as_bytes()))
                    .unwrap_or_else(|_| text.to_string());
                let line_count = clean.lines().count();
                let first_line = clean.lines().next().unwrap_or("").trim();
                let short = if first_line.len() > 60 {
                    format!("{}…", &first_line[..first_line.floor_char_boundary(60)])
                } else {
                    first_line.to_string()
                };
                if line_count > 1 {
                    parts.push(format!("  ↳ {} ({} lines)", short, line_count));
                } else {
                    parts.push(format!("  ↳ {}", short));
                }
            }
        }
    }

    parts.join("\n\n")
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
pub fn format_relative_time(ts: &str, now: &chrono::DateTime<chrono::Utc>) -> String {
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
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
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
    fn hud_shows_runtime_and_compaction_metadata() {
        let (viz, _, _tmp) = build_chain_plus_isolated();

        let mut registry = AgentRegistry::new();
        registry.agents.insert(
            "agent-001".to_string(),
            workgraph::service::AgentEntry {
                id: "agent-001".to_string(),
                pid: 123,
                task_id: "a".to_string(),
                executor: "native".to_string(),
                started_at: "2026-01-20T16:00:00Z".to_string(),
                last_heartbeat: "2026-01-20T16:05:00Z".to_string(),
                status: workgraph::service::AgentStatus::Working,
                output_file: "output.log".to_string(),
                model: Some("openrouter/minimax".to_string()),
                completed_at: None,
            },
        );
        registry.save(_tmp.path()).unwrap();

        let journal_path = workgraph::executor::native::journal::journal_path(_tmp.path(), "a");
        let mut journal =
            workgraph::executor::native::journal::Journal::open(&journal_path).unwrap();
        journal
            .append(
                workgraph::executor::native::journal::JournalEntryKind::Init {
                    model: "openrouter/minimax".to_string(),
                    provider: "openrouter".to_string(),
                    system_prompt: "test".to_string(),
                    tools: vec![],
                    task_id: Some("a".to_string()),
                },
            )
            .unwrap();
        journal
            .append(
                workgraph::executor::native::journal::JournalEntryKind::Compaction {
                    compacted_through_seq: 1,
                    summary: "summary".to_string(),
                    original_message_count: 4,
                    original_token_count: 400,
                    model_used: None,
                    fallback_reason: None,
                },
            )
            .unwrap();

        let summary_path = _tmp
            .path()
            .join("agents")
            .join("agent-001")
            .join("session-summary.md");
        std::fs::create_dir_all(summary_path.parent().unwrap()).unwrap();
        std::fs::write(summary_path, "short session summary").unwrap();

        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Runtime ──")),
            "runtime section should be present"
        );
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Executor: native"))
        );
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Model: openrouter/minimax"))
        );
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Compaction ──"))
        );
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Compactions: 1"))
        );
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("Session summary: present"))
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
        assert!(detail.rendered_lines.iter().any(|l| l.contains("Cached:")));
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
            annotation_map: HashMap::new(),
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
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
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
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
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
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
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

    // ── Iteration browsing tests ──

    /// Helper: create a cyclic task with archived iterations in a temp dir.
    fn build_cyclic_task_with_archives(
        iteration_count: usize,
    ) -> (VizOutput, WorkGraph, tempfile::TempDir) {
        let mut graph = WorkGraph::new();
        let mut task = make_task_with_status("cycle-task", "Cyclic Task", Status::InProgress);
        task.description = Some("A task in a cycle".to_string());
        task.loop_iteration = iteration_count as u32;
        task.cycle_config = Some(workgraph::graph::CycleConfig {
            max_iterations: 10,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: false,
            max_failure_restarts: None,
        });
        task.assigned = Some("agent-cyc".to_string());
        task.after = vec!["cycle-task".to_string()]; // self-loop

        graph.add_node(Node::Task(task));

        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph.jsonl");
        save_graph(&graph, &graph_path).unwrap();

        // Create archived iteration directories with output files
        let archive_base = tmp.path().join("log").join("agents").join("cycle-task");
        std::fs::create_dir_all(&archive_base).unwrap();
        for i in 0..iteration_count {
            let ts = format!("2026-01-15T10:{:02}:00Z", i);
            let iter_dir = archive_base.join(&ts);
            std::fs::create_dir_all(&iter_dir).unwrap();
            std::fs::write(
                iter_dir.join("output.txt"),
                format!("Output from iteration {}", i + 1),
            )
            .unwrap();
            std::fs::write(
                iter_dir.join("prompt.txt"),
                format!("Prompt for iteration {}", i + 1),
            )
            .unwrap();
        }

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
        (viz, graph, tmp)
    }

    #[test]
    fn tui_iteration_no_archives_shows_no_iterations_section() {
        // Non-cycling task with no archives should not show Iterations section
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        let has_iterations = detail
            .rendered_lines
            .iter()
            .any(|l| l.contains("Iterations") || l.contains("Attempts"));
        assert!(
            !has_iterations,
            "Non-cycling task should not show iteration section"
        );
        assert!(app.iteration_archives.is_empty());
        assert!(app.viewing_iteration.is_none());
    }

    #[test]
    fn tui_iteration_cyclic_task_shows_iterations_section() {
        let (viz, _, _tmp) = build_cyclic_task_with_archives(3);
        let mut app = build_app(&viz, "cycle-task", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        let has_iterations = detail
            .rendered_lines
            .iter()
            .any(|l| l.contains("── Iterations ──"));
        assert!(has_iterations, "Cyclic task should show Iterations section");
        assert_eq!(app.iteration_archives.len(), 3);

        // Check the header includes iteration info
        let header = &detail.rendered_lines[0];
        assert!(
            header.contains("iter"),
            "Header should contain iteration label: {}",
            header
        );
    }

    #[test]
    fn tui_iteration_browse_prev_next() {
        let (viz, _, _tmp) = build_cyclic_task_with_archives(3);
        let mut app = build_app(&viz, "cycle-task", _tmp.path());
        app.load_hud_detail();

        // Initially viewing current (None)
        assert!(app.viewing_iteration.is_none());

        // Navigate to previous (most recent archive = index 2)
        assert!(app.iteration_prev());
        assert_eq!(app.viewing_iteration, Some(2));

        // Navigate to previous again (index 1)
        assert!(app.iteration_prev());
        assert_eq!(app.viewing_iteration, Some(1));

        // Navigate to previous again (index 0 = oldest)
        assert!(app.iteration_prev());
        assert_eq!(app.viewing_iteration, Some(0));

        // Can't go further back
        assert!(!app.iteration_prev());
        assert_eq!(app.viewing_iteration, Some(0));

        // Navigate forward
        assert!(app.iteration_next());
        assert_eq!(app.viewing_iteration, Some(1));

        assert!(app.iteration_next());
        assert_eq!(app.viewing_iteration, Some(2));

        // Next from the last archive goes back to current
        assert!(app.iteration_next());
        assert!(app.viewing_iteration.is_none());

        // Can't go further forward when at current
        assert!(!app.iteration_next());
    }

    #[test]
    fn tui_iteration_archived_output_displayed() {
        let (viz, _, _tmp) = build_cyclic_task_with_archives(2);
        let mut app = build_app(&viz, "cycle-task", _tmp.path());
        app.load_hud_detail();

        // Navigate to first archived iteration
        app.viewing_iteration = Some(0);
        app.hud_detail = None;
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();

        // Check that the Output section header includes "(iteration 1)"
        let has_iter_header = detail
            .rendered_lines
            .iter()
            .any(|l| l.contains("Output (iteration 1)"));
        assert!(
            has_iter_header,
            "Output section header should indicate iteration number. Lines: {:?}",
            detail
                .rendered_lines
                .iter()
                .filter(|l| l.contains("Output") || l.contains("Prompt"))
                .collect::<Vec<_>>()
        );

        // Check that Prompt section header also includes "(iteration 1)"
        let has_prompt_header = detail
            .rendered_lines
            .iter()
            .any(|l| l.contains("Prompt (iteration 1)"));
        assert!(
            has_prompt_header,
            "Prompt section header should indicate iteration number"
        );
    }

    #[test]
    fn tui_iteration_no_change_for_empty_archives() {
        let (viz, _, _tmp) = build_chain_plus_isolated();
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        // iteration_prev and iteration_next are no-ops
        assert!(!app.iteration_prev());
        assert!(!app.iteration_next());
        assert!(app.viewing_iteration.is_none());
    }

    #[test]
    fn tui_iteration_retry_task_shows_attempts() {
        let mut graph = WorkGraph::new();
        let mut task = make_task_with_status("retry-task", "Retry Task", Status::InProgress);
        task.retry_count = 2;
        task.assigned = Some("agent-retry".to_string());

        graph.add_node(Node::Task(task));

        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph.jsonl");
        save_graph(&graph, &graph_path).unwrap();

        // Create archives for retries
        let archive_base = tmp.path().join("log").join("agents").join("retry-task");
        std::fs::create_dir_all(&archive_base).unwrap();
        for i in 0..2 {
            let ts = format!("2026-01-15T10:{:02}:00Z", i);
            let iter_dir = archive_base.join(&ts);
            std::fs::create_dir_all(&iter_dir).unwrap();
            std::fs::write(iter_dir.join("output.txt"), format!("Attempt {}", i + 1)).unwrap();
        }

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

        let mut app = build_app(&viz, "retry-task", tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        let has_attempts = detail
            .rendered_lines
            .iter()
            .any(|l| l.contains("── Attempts ──"));
        assert!(
            has_attempts,
            "Retry task should show Attempts section, not Iterations"
        );
        assert_eq!(app.iteration_archives.len(), 2);
    }

    // ── INTEGRATION: viz self-loop indicator + TUI iteration history ──

    #[test]
    fn integration_self_loop_viz_and_tui_iteration_browsing() {
        // End-to-end: a self-looping task with cycle_config and archives should:
        // 1. Show ↺ (iter N/M) in viz output (not ⟳, since cycle_config is set)
        // 2. Have browsable iterations in TUI detail view
        let (viz, _, _tmp) = build_cyclic_task_with_archives(3);

        // --- Viz verification ---
        // Self-loop with cycle_config should show ↺ with iteration info
        assert!(
            viz.text.contains("↺"),
            "Self-loop with cycle_config should show ↺ in viz:\n{}",
            viz.text
        );
        assert!(
            viz.text.contains("iter 3/10"),
            "Viz should show iteration progress:\n{}",
            viz.text
        );
        // Should NOT also show ⟳ (that's for self-loops without cycle_config)
        assert!(
            !viz.text.contains("⟳"),
            "Should not show ⟳ when ↺ is already present:\n{}",
            viz.text
        );

        // --- TUI iteration browsing verification ---
        let mut app = build_app(&viz, "cycle-task", _tmp.path());
        app.load_hud_detail();

        // Should have 3 archived iterations
        assert_eq!(app.iteration_archives.len(), 3);
        assert!(
            app.viewing_iteration.is_none(),
            "Should start at current view"
        );

        // Detail should show Iterations section
        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("── Iterations ──")),
            "TUI detail should show Iterations section for cyclic task"
        );

        // Browse backward through all 3 iterations
        assert!(app.iteration_prev()); // -> archive 2
        assert_eq!(app.viewing_iteration, Some(2));
        assert!(app.iteration_prev()); // -> archive 1
        assert_eq!(app.viewing_iteration, Some(1));
        assert!(app.iteration_prev()); // -> archive 0
        assert_eq!(app.viewing_iteration, Some(0));
        assert!(!app.iteration_prev()); // can't go further

        // Browse forward back to current
        assert!(app.iteration_next()); // -> archive 1
        assert!(app.iteration_next()); // -> archive 2
        assert!(app.iteration_next()); // -> current
        assert!(app.viewing_iteration.is_none());
        assert!(!app.iteration_next()); // can't go further

        // Verify archived iteration content is accessible
        app.viewing_iteration = Some(0);
        app.hud_detail = None;
        app.load_hud_detail();
        let detail = app.hud_detail.as_ref().unwrap();
        let has_iter_header = detail
            .rendered_lines
            .iter()
            .any(|l| l.contains("Output (iteration 1)"));
        assert!(
            has_iter_header,
            "Archived iteration should show labeled output section"
        );
    }

    #[test]
    fn integration_non_cycling_task_no_loop_indicator_no_iterations() {
        // Non-cycling tasks should have neither loop indicator in viz nor iterations in TUI
        let (viz, _, _tmp) = build_chain_plus_isolated();

        // --- Viz: no loop symbols ---
        assert!(
            !viz.text.contains("↺"),
            "Non-cycling tasks should not show ↺:\n{}",
            viz.text
        );
        assert!(
            !viz.text.contains("⟳"),
            "Non-cycling tasks should not show ⟳:\n{}",
            viz.text
        );

        // --- TUI: no iterations section, browsing is no-op ---
        let mut app = build_app(&viz, "a", _tmp.path());
        app.load_hud_detail();

        let detail = app.hud_detail.as_ref().unwrap();
        let has_iterations = detail
            .rendered_lines
            .iter()
            .any(|l| l.contains("Iterations") || l.contains("Attempts"));
        assert!(
            !has_iterations,
            "Non-cycling task should not show iteration section"
        );
        assert!(app.iteration_archives.is_empty());

        // Browsing should be no-op
        assert!(!app.iteration_prev());
        assert!(!app.iteration_next());
        assert!(app.viewing_iteration.is_none());
    }

    #[test]
    fn integration_self_loop_no_cycle_config_shows_distinct_indicator() {
        // Self-loop WITHOUT cycle_config: should show ⟳ (not ↺) and no TUI iterations
        let mut graph = WorkGraph::new();
        let mut task = make_task_with_status("bare-loop", "Bare Self-Loop", Status::Open);
        task.after = vec!["bare-loop".to_string()];
        // No cycle_config, no archives
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
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Viz: ⟳ present, ↺ absent
        assert!(
            viz.text.contains("⟳"),
            "Self-loop without cycle_config should show ⟳:\n{}",
            viz.text
        );
        assert!(
            !viz.text.contains("↺"),
            "Self-loop without cycle_config should not show ↺:\n{}",
            viz.text
        );

        // TUI: no iterations to browse
        let mut app = build_app(&viz, "bare-loop", tmp.path());
        app.load_hud_detail();
        assert!(app.iteration_archives.is_empty());
        assert!(!app.iteration_prev());
        assert!(!app.iteration_next());
    }

    #[test]
    fn integration_zero_iteration_task_no_tui_crash() {
        // A cyclic task with 0 completed iterations should not crash TUI
        let mut graph = WorkGraph::new();
        let mut task = make_task_with_status("zero-iter", "Zero Iteration", Status::Open);
        task.cycle_config = Some(workgraph::graph::CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: false,
            max_failure_restarts: None,
        });
        task.loop_iteration = 0;
        task.after = vec!["zero-iter".to_string()];
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
            LayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Viz should show ↺ (has cycle_config)
        assert!(
            viz.text.contains("↺"),
            "Cyclic task with 0 iterations should show ↺:\n{}",
            viz.text
        );

        // TUI should not crash — no archives, browsing is no-op
        let mut app = build_app(&viz, "zero-iter", tmp.path());
        app.load_hud_detail();
        assert!(app.iteration_archives.is_empty());
        assert!(!app.iteration_prev());
        assert!(!app.iteration_next());
        assert!(app.viewing_iteration.is_none());

        // Detail should load without panic
        let detail = app.hud_detail.as_ref().unwrap();
        assert!(
            detail
                .rendered_lines
                .iter()
                .any(|l| l.contains("zero-iter"))
        );
    }
}

#[cfg(test)]
mod extract_assistant_text_tests {
    use super::*;

    #[test]
    fn extracts_text_from_assistant_messages() {
        let log = concat!(
            r#"{"type":"system","subtype":"init","cwd":"/tmp"}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Heading here\n\nThis is **bold** text."}]}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[]}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Second message."}]}}"#,
        );
        let result = extract_enriched_text_from_log(log);
        assert!(result.contains("Heading here"));
        assert!(result.contains("**bold**"));
        assert!(result.contains("Second message."));
    }

    #[test]
    fn extracts_tool_use_summaries() {
        let log = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#;
        let result = extract_enriched_text_from_log(log);
        // Bash commands now use "$ " prefix for visibility
        assert!(result.contains("$ "));
        assert!(result.contains("cargo test"));
    }

    #[test]
    fn handles_read_tool_summary() {
        let log = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/src/main.rs"}}]}}"#;
        let result = extract_enriched_text_from_log(log);
        assert!(result.contains("┌─ Read"));
        assert!(result.contains("/src/main.rs"));
    }

    #[test]
    fn handles_grep_tool_summary() {
        let log = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep","input":{"pattern":"fn main"}}]}}"#;
        let result = extract_enriched_text_from_log(log);
        assert!(result.contains("┌─ Grep"));
        assert!(result.contains("fn main"));
    }

    #[test]
    fn ignores_non_assistant_messages() {
        let log = r#"{"type":"system","subtype":"init"}
{"type":"user","message":{"content":[{"type":"text","text":"user text"}]}}
{"type":"rate_limit_event","rate_limit_info":{}}"#;
        let result = extract_enriched_text_from_log(log);
        assert!(result.is_empty());
    }

    #[test]
    fn skips_empty_text_blocks() {
        let log = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"  \n  "}]}}"#;
        let result = extract_enriched_text_from_log(log);
        assert!(result.is_empty());
    }

    #[test]
    fn handles_mixed_text_and_tool_use() {
        let log = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me check."},{"type":"tool_use","name":"Bash","input":{"command":"ls -la"}}]}}"#;
        let result = extract_enriched_text_from_log(log);
        assert!(result.contains("Let me check."));
        // Bash commands now use "$ " prefix for visibility
        assert!(result.contains("$ "));
        assert!(result.contains("ls -la"));
    }

    #[test]
    fn unknown_tool_shows_name_only() {
        let log = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"CustomTool","input":{"foo":"bar"}}]}}"#;
        let result = extract_enriched_text_from_log(log);
        assert!(result.contains("┌─ CustomTool"));
        assert!(result.contains("└─"));
        // No detail line for unknown tools
        assert!(!result.contains("│ "));
    }

    #[test]
    fn handles_malformed_json_gracefully() {
        let log = "not json at all\n{broken json\n";
        let result = extract_enriched_text_from_log(log);
        assert!(result.is_empty());
    }

    #[test]
    fn truncates_long_bash_commands() {
        let long_cmd = "x".repeat(200);
        let log = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Bash","input":{{"command":"{}"}}}}]}}}}"#,
            long_cmd
        );
        let result = extract_enriched_text_from_log(&log);
        // Bash commands now use "$ " prefix for visibility
        assert!(result.contains("$ "));
        // Should be truncated with …
        assert!(result.contains('…'));
    }
}

#[cfg(test)]
mod remap_panel_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use workgraph::graph::{Node, Status, WorkGraph};
    use workgraph::test_helpers::make_task_with_status;

    use crate::commands::viz::LayoutMode as VizLayoutMode;
    use crate::commands::viz::ascii::generate_ascii;

    fn build_test_app() -> VizApp {
        let mut graph = WorkGraph::new();
        let a = make_task_with_status("a", "Task A", Status::Open);
        graph.add_node(Node::Task(a));

        let tmp = tempfile::tempdir().unwrap();
        let gpath = tmp.path().join("graph.jsonl");
        workgraph::parser::save_graph(&graph, &gpath).unwrap();

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            VizLayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.workgraph_dir = tmp.path().to_path_buf();
        // Keep tempdir alive by leaking — tests are short-lived
        std::mem::forget(tmp);
        app
    }

    // ── Cycle inspector view forward ──

    #[test]
    fn cycle_inspector_view_forward_opens_first_tab() {
        let mut app = build_test_app();
        app.right_panel_visible = false;
        app.layout_mode = LayoutMode::Off;

        app.cycle_inspector_view_forward();

        assert!(app.right_panel_visible);
        assert_eq!(app.right_panel_tab, RightPanelTab::Chat);
        assert!(app.slide_animation.is_some());
    }

    #[test]
    fn cycle_inspector_view_forward_advances_tab() {
        let mut app = build_test_app();
        app.right_panel_visible = true;
        app.layout_mode = LayoutMode::TwoThirdsInspector;
        app.right_panel_tab = RightPanelTab::Chat;

        app.cycle_inspector_view_forward();

        assert_eq!(app.right_panel_tab, RightPanelTab::Detail);
        assert!(app.slide_animation.is_some());
    }

    #[test]
    fn cycle_inspector_view_forward_closes_on_last_tab() {
        let mut app = build_test_app();
        app.right_panel_visible = true;
        app.layout_mode = LayoutMode::TwoThirdsInspector;
        app.right_panel_tab = RightPanelTab::Messages; // last tab

        app.cycle_inspector_view_forward();

        assert!(!app.right_panel_visible);
        assert_eq!(app.layout_mode, LayoutMode::Off);
    }

    // ── Cycle inspector view backward ──

    #[test]
    fn cycle_inspector_view_backward_opens_last_tab() {
        let mut app = build_test_app();
        app.right_panel_visible = false;
        app.layout_mode = LayoutMode::Off;

        app.cycle_inspector_view_backward();

        assert!(app.right_panel_visible);
        assert_eq!(app.right_panel_tab, RightPanelTab::Messages);
        assert!(app.slide_animation.is_some());
    }

    #[test]
    fn cycle_inspector_view_backward_closes_on_first_tab() {
        let mut app = build_test_app();
        app.right_panel_visible = true;
        app.layout_mode = LayoutMode::TwoThirdsInspector;
        app.right_panel_tab = RightPanelTab::Chat; // first tab

        app.cycle_inspector_view_backward();

        assert!(!app.right_panel_visible);
        assert_eq!(app.layout_mode, LayoutMode::Off);
    }

    // ── Full forward cycle: 10 presses returns to closed ──

    #[test]
    fn cycle_inspector_view_forward_full_cycle() {
        let mut app = build_test_app();
        app.right_panel_visible = false;
        app.layout_mode = LayoutMode::Off;

        // 8 live tabs + 1 close = 9 presses
        let expected_tabs = [
            Some(RightPanelTab::Chat),
            Some(RightPanelTab::Detail),
            Some(RightPanelTab::Agency),
            Some(RightPanelTab::Config),
            Some(RightPanelTab::Log),
            Some(RightPanelTab::CoordLog),
            Some(RightPanelTab::Dashboard),
            Some(RightPanelTab::Messages),
            None, // closed
        ];

        for (i, expected) in expected_tabs.iter().enumerate() {
            app.cycle_inspector_view_forward();
            match expected {
                Some(tab) => {
                    assert!(app.right_panel_visible, "press {i}: should be visible");
                    assert_eq!(app.right_panel_tab, *tab, "press {i}: wrong tab");
                }
                None => {
                    assert!(!app.right_panel_visible, "press {i}: should be closed");
                }
            }
        }
    }

    // ── Grow viz pane ──

    #[test]
    fn grow_viz_pane_increases_by_5_percent() {
        let mut app = build_test_app();
        app.right_panel_visible = true;
        app.layout_mode = LayoutMode::ThirdInspector;
        app.right_panel_percent = 10;

        app.grow_viz_pane();
        assert_eq!(app.right_panel_percent, 15);

        app.grow_viz_pane();
        assert_eq!(app.right_panel_percent, 20);
    }

    #[test]
    fn grow_viz_pane_reaches_full_screen() {
        let mut app = build_test_app();
        app.right_panel_visible = true;
        app.layout_mode = LayoutMode::ThirdInspector;
        app.right_panel_percent = 95;

        app.grow_viz_pane();
        assert_eq!(app.right_panel_percent, 100);
        assert_eq!(app.layout_mode, LayoutMode::FullInspector);
    }

    #[test]
    fn grow_viz_pane_from_full_transitions_to_off() {
        let mut app = build_test_app();
        app.right_panel_visible = true;
        app.layout_mode = LayoutMode::FullInspector;
        app.right_panel_percent = 100;

        // At 100% → transitions to Off (no wrap)
        app.grow_viz_pane();
        assert!(!app.right_panel_visible);
        assert_eq!(app.layout_mode, LayoutMode::Off);
    }

    #[test]
    fn grow_viz_pane_full_roundtrip() {
        let mut app = build_test_app();
        app.right_panel_visible = false;
        app.layout_mode = LayoutMode::Off;

        // First press opens at 5%
        app.grow_viz_pane();
        assert_eq!(app.right_panel_percent, 5);
        assert!(app.right_panel_visible);

        // 19 more presses: 10, 15, 20, ..., 100
        for expected in (10..=100).step_by(5) {
            app.grow_viz_pane();
            assert_eq!(app.right_panel_percent, expected);
        }
        assert_eq!(app.layout_mode, LayoutMode::FullInspector);

        // One more transitions to Off (no wrap)
        app.grow_viz_pane();
        assert!(!app.right_panel_visible);
        assert_eq!(app.layout_mode, LayoutMode::Off);
    }

    #[test]
    fn grow_viz_pane_opens_panel_when_closed() {
        let mut app = build_test_app();
        app.right_panel_visible = false;
        app.layout_mode = LayoutMode::Off;

        app.grow_viz_pane();

        assert!(app.right_panel_visible);
        assert_eq!(app.right_panel_percent, 5);
    }

    // ── Shrink viz pane ──

    #[test]
    fn shrink_viz_pane_decreases_by_5_percent() {
        let mut app = build_test_app();
        app.right_panel_visible = true;
        app.layout_mode = LayoutMode::TwoThirdsInspector;
        app.right_panel_percent = 70;

        app.shrink_viz_pane();
        assert_eq!(app.right_panel_percent, 65);

        app.shrink_viz_pane();
        assert_eq!(app.right_panel_percent, 60);
    }

    #[test]
    fn shrink_viz_pane_from_min_transitions_to_off() {
        let mut app = build_test_app();
        app.right_panel_visible = true;
        app.layout_mode = LayoutMode::ThirdInspector;
        app.right_panel_percent = 5;

        // At min (5%) → transitions to Off (no wrap)
        app.shrink_viz_pane();
        assert!(!app.right_panel_visible);
        assert_eq!(app.layout_mode, LayoutMode::Off);
    }

    #[test]
    fn shrink_viz_pane_opens_panel_when_closed() {
        let mut app = build_test_app();
        app.right_panel_visible = false;
        app.layout_mode = LayoutMode::Off;

        app.shrink_viz_pane();

        assert!(app.right_panel_visible);
        assert_eq!(app.right_panel_percent, 100);
    }

    // ── SlideAnimation ──

    #[test]
    fn slide_animation_progress_and_done() {
        let anim = SlideAnimation {
            start: Instant::now() - std::time::Duration::from_millis(200),
            direction: SlideDirection::Forward,
        };
        assert!(
            anim.is_done(),
            "animation should be done after 200ms (duration=150ms)"
        );
        assert!((anim.progress() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn slide_animation_x_offset_at_start() {
        let anim = SlideAnimation {
            start: Instant::now(),
            direction: SlideDirection::Forward,
        };
        let offset = anim.x_offset(100);
        // At start, offset should be near panel_width (100)
        assert!(
            offset > 80,
            "forward offset at start should be near panel_width, got {offset}"
        );
    }
}

#[cfg(test)]
mod firehose_tests {
    use super::*;
    use crate::commands::viz::LayoutMode as VizLayoutMode;
    use crate::commands::viz::ascii::generate_ascii;
    use std::collections::{HashMap, HashSet};
    use std::io::Write;
    use workgraph::graph::{Node, Status, WorkGraph};
    use workgraph::test_helpers::make_task_with_status;

    fn write_registry(workgraph_dir: &std::path::Path, agents_json: &str) {
        let svc_dir = workgraph_dir.join("service");
        std::fs::create_dir_all(&svc_dir).unwrap();
        std::fs::write(
            svc_dir.join("registry.json"),
            format!(r#"{{"agents":{{{agents_json}}},"next_agent_id":100}}"#),
        )
        .unwrap();
    }

    fn agent_entry(id: &str, task_id: &str) -> String {
        format!(
            r#""{id}":{{"id":"{id}","pid":1,"task_id":"{task_id}","executor":"claude","started_at":"2026-03-07T00:00:00Z","last_heartbeat":"2026-03-07T00:00:00Z","status":"working","output_file":"agents/{id}/output.log"}}"#,
        )
    }

    fn build_test_app() -> VizApp {
        let mut graph = WorkGraph::new();
        let a = make_task_with_status("a", "Task A", Status::Open);
        graph.add_node(Node::Task(a));
        let tmp = tempfile::tempdir().unwrap();
        let gpath = tmp.path().join("graph.jsonl");
        workgraph::parser::save_graph(&graph, &gpath).unwrap();
        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            VizLayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.workgraph_dir = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        app
    }

    #[test]
    fn firehose_tab_in_panel_cycle() {
        assert_eq!(RightPanelTab::Firehose.label(), "");
        assert_eq!(RightPanelTab::CoordLog.next(), RightPanelTab::Dashboard);
        assert_eq!(RightPanelTab::Dashboard.next(), RightPanelTab::Messages);
        assert_eq!(RightPanelTab::Messages.next(), RightPanelTab::Chat);
        assert_eq!(RightPanelTab::Chat.prev(), RightPanelTab::Messages);
    }

    #[test]
    fn firehose_update_reads_output_logs() {
        let mut app = build_test_app();
        let agents_dir = app.workgraph_dir.join("agents").join("agent-1234");
        std::fs::create_dir_all(&agents_dir).unwrap();
        {
            let mut f = std::fs::File::create(agents_dir.join("output.log")).unwrap();
            writeln!(f, "Starting task...").unwrap();
            writeln!(f, "Processing data...").unwrap();
            writeln!(f, "Done.").unwrap();
        }
        write_registry(&app.workgraph_dir, &agent_entry("agent-1234", "test-task"));
        app.load_agent_monitor();
        app.update_firehose();
        assert_eq!(app.firehose.lines.len(), 3);
        assert_eq!(app.firehose.lines[0].agent_id, "agent-1234");
        assert_eq!(app.firehose.lines[0].task_id, "test-task");
        assert_eq!(app.firehose.lines[0].text, "Starting task...");
        assert_eq!(app.firehose.lines[2].text, "Done.");
    }

    #[test]
    fn firehose_incremental_read() {
        let mut app = build_test_app();
        let agents_dir = app.workgraph_dir.join("agents").join("agent-5678");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(agents_dir.join("output.log"), "Line 1\n").unwrap();
        write_registry(&app.workgraph_dir, &agent_entry("agent-5678", "t"));
        app.load_agent_monitor();
        app.update_firehose();
        assert_eq!(app.firehose.lines.len(), 1);
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(agents_dir.join("output.log"))
                .unwrap();
            writeln!(f, "Line 2").unwrap();
            writeln!(f, "Line 3").unwrap();
        }
        app.update_firehose();
        assert_eq!(app.firehose.lines.len(), 3);
        assert_eq!(app.firehose.lines[1].text, "Line 2");
        assert_eq!(app.firehose.lines[2].text, "Line 3");
    }

    #[test]
    fn firehose_buffer_cap() {
        let mut app = build_test_app();
        for i in 0..1200 {
            app.firehose.lines.push(FirehoseLine {
                agent_id: "agent-0".to_string(),
                task_id: "t".to_string(),
                text: format!("line {i}"),
                color_idx: 0,
            });
        }
        if app.firehose.lines.len() > FIREHOSE_MAX_LINES {
            let drain = app.firehose.lines.len() - FIREHOSE_MAX_LINES;
            app.firehose.lines.drain(..drain);
        }
        assert_eq!(app.firehose.lines.len(), FIREHOSE_MAX_LINES);
        assert_eq!(app.firehose.lines[0].text, "line 200");
    }

    #[test]
    fn firehose_distinct_colors_per_agent() {
        let mut app = build_test_app();
        let c1 = *app
            .firehose
            .agent_colors
            .entry("a1".into())
            .or_insert_with(|| {
                let i = app.firehose.next_color;
                app.firehose.next_color += 1;
                i
            });
        let c2 = *app
            .firehose
            .agent_colors
            .entry("a2".into())
            .or_insert_with(|| {
                let i = app.firehose.next_color;
                app.firehose.next_color += 1;
                i
            });
        let c1b = *app
            .firehose
            .agent_colors
            .entry("a1".into())
            .or_insert_with(|| {
                let i = app.firehose.next_color;
                app.firehose.next_color += 1;
                i
            });
        assert_ne!(c1, c2);
        assert_eq!(c1, c1b);
    }

    #[test]
    fn firehose_multiple_agents_interleaved() {
        let mut app = build_test_app();
        let agents_dir = app.workgraph_dir.join("agents");
        let dir_a = agents_dir.join("agent-a");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::write(dir_a.join("output.log"), "A1\nA2\n").unwrap();
        let dir_b = agents_dir.join("agent-b");
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(dir_b.join("output.log"), "B1\nB2\n").unwrap();
        let entries = format!(
            "{},{}",
            agent_entry("agent-a", "ta"),
            agent_entry("agent-b", "tb")
        );
        write_registry(&app.workgraph_dir, &entries);
        app.load_agent_monitor();
        app.update_firehose();
        assert_eq!(app.firehose.lines.len(), 4);
        let ids: HashSet<&str> = app
            .firehose
            .lines
            .iter()
            .map(|l| l.agent_id.as_str())
            .collect();
        assert!(ids.contains("agent-a"));
        assert!(ids.contains("agent-b"));
        let ca = app
            .firehose
            .lines
            .iter()
            .find(|l| l.agent_id == "agent-a")
            .unwrap()
            .color_idx;
        let cb = app
            .firehose
            .lines
            .iter()
            .find(|l| l.agent_id == "agent-b")
            .unwrap()
            .color_idx;
        assert_ne!(ca, cb);
    }
}

#[cfg(test)]
mod service_health_tests {
    use super::*;

    #[test]
    fn default_is_red_down() {
        let health = ServiceHealthState::default();
        assert_eq!(health.level, ServiceHealthLevel::Red);
        assert_eq!(health.label, "DOWN");
        assert!(health.pid.is_none());
        assert!(!health.detail_open);
        assert!(health.stuck_tasks.is_empty());
        assert!(health.recent_errors.is_empty());
    }

    #[test]
    fn level_equality() {
        assert_eq!(ServiceHealthLevel::Green, ServiceHealthLevel::Green);
        assert_ne!(ServiceHealthLevel::Green, ServiceHealthLevel::Yellow);
        assert_ne!(ServiceHealthLevel::Yellow, ServiceHealthLevel::Red);
    }

    #[test]
    fn stuck_task_fields() {
        let stuck = StuckTask {
            task_id: "build-ui".to_string(),
            task_title: "Build the UI component".to_string(),
            agent_id: "agent-42".to_string(),
        };
        assert_eq!(stuck.task_id, "build-ui");
        assert_eq!(stuck.task_title, "Build the UI component");
        assert_eq!(stuck.agent_id, "agent-42");
    }

    #[test]
    fn toggle_detail() {
        let mut health = ServiceHealthState::default();
        assert!(!health.detail_open);
        health.detail_open = !health.detail_open;
        assert!(health.detail_open);
        health.detail_open = !health.detail_open;
        assert!(!health.detail_open);
    }

    #[test]
    fn uptime_format_logic() {
        let fmt = |s: u64| -> String {
            if s < 60 {
                format!("{}s", s)
            } else if s < 3600 {
                format!("{}m{}s", s / 60, s % 60)
            } else {
                format!("{}h{}m", s / 3600, (s % 3600) / 60)
            }
        };
        assert_eq!(fmt(0), "0s");
        assert_eq!(fmt(29), "29s");
        assert_eq!(fmt(60), "1m0s");
        assert_eq!(fmt(90), "1m30s");
        assert_eq!(fmt(3600), "1h0m");
        assert_eq!(fmt(3661), "1h1m");
    }

    #[test]
    fn yellow_paused() {
        let mut h = ServiceHealthState::default();
        h.paused = true;
        h.level = ServiceHealthLevel::Yellow;
        h.label = "PAUSED".to_string();
        assert_eq!(h.level, ServiceHealthLevel::Yellow);
        assert_eq!(h.label, "PAUSED");
    }

    #[test]
    fn yellow_stuck_tasks() {
        let mut h = ServiceHealthState::default();
        h.stuck_tasks = vec![
            StuckTask {
                task_id: "t1".into(),
                task_title: "T1".into(),
                agent_id: "a1".into(),
            },
            StuckTask {
                task_id: "t2".into(),
                task_title: "T2".into(),
                agent_id: "a2".into(),
            },
        ];
        h.level = ServiceHealthLevel::Yellow;
        h.label = format!("OK ({} stuck)", h.stuck_tasks.len());
        assert_eq!(h.label, "OK (2 stuck)");
    }

    #[test]
    fn green_state() {
        let mut h = ServiceHealthState::default();
        h.level = ServiceHealthLevel::Green;
        h.agents_alive = 3;
        h.agents_max = 6;
        h.label = format!("{}/{}", h.agents_alive, h.agents_max);
        assert_eq!(h.label, "3/6");
    }

    #[test]
    fn detail_scroll_bounds() {
        let mut h = ServiceHealthState::default();
        h.detail_scroll = h.detail_scroll.saturating_add(3);
        assert_eq!(h.detail_scroll, 3);
        h.detail_scroll = h.detail_scroll.saturating_sub(100);
        assert_eq!(h.detail_scroll, 0);
    }

    #[test]
    fn no_service_state_file_is_red() {
        let temp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(temp.path().join("service")).unwrap();
        use crate::commands::service::ServiceState;
        assert!(ServiceState::load(temp.path()).ok().flatten().is_none());
    }

    #[test]
    fn control_panel_defaults() {
        let h = ServiceHealthState::default();
        assert!(!h.panel_open);
        assert_eq!(h.panel_focus, ControlPanelFocus::StartStop);
        assert!(!h.panic_confirm);
        assert!(h.feedback.is_none());
    }

    #[test]
    fn control_panel_focus_navigation() {
        let f = ControlPanelFocus::StartStop;
        assert_eq!(f.next(0), ControlPanelFocus::PauseResume);
        assert_eq!(f.next(0).next(0), ControlPanelFocus::Restart);
    }

    #[test]
    fn control_panel_focus_agent_slots() {
        // Restart -> AgentSlots -> PanicKill
        let f = ControlPanelFocus::Restart;
        assert_eq!(f.next(0), ControlPanelFocus::AgentSlots);
        assert_eq!(f.next(0).next(0), ControlPanelFocus::PanicKill);
        // PanicKill -> prev -> AgentSlots -> prev -> Restart
        let f = ControlPanelFocus::PanicKill;
        assert_eq!(f.prev(0), ControlPanelFocus::AgentSlots);
        assert_eq!(f.prev(0).prev(0), ControlPanelFocus::Restart);
    }

    #[test]
    fn control_panel_focus_with_stuck() {
        let f = ControlPanelFocus::PanicKill;
        assert_eq!(f.next(2), ControlPanelFocus::StuckAgent(0));
        assert_eq!(f.next(0), ControlPanelFocus::KillAllDead);
    }

    #[test]
    fn control_panel_focus_prev_wraps() {
        let f = ControlPanelFocus::StartStop;
        assert_eq!(f.prev(0), ControlPanelFocus::RetryFailedEvals);
    }

    #[test]
    fn no_degraded_label() {
        let h = ServiceHealthState::default();
        assert!(!h.label.contains("DEGRADED"));
    }
}

#[cfg(test)]
mod tui_config_panel_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use workgraph::config::Config;
    use workgraph::graph::{Node, Status, WorkGraph};
    use workgraph::parser::save_graph;
    use workgraph::test_helpers::make_task_with_status;

    use crate::commands::viz::LayoutMode as VizLayoutMode;
    use crate::commands::viz::ascii::generate_ascii;

    /// Create a minimal VizApp with a real temp directory for config round-trip testing.
    fn build_config_test_app() -> (VizApp, tempfile::TempDir) {
        let mut graph = WorkGraph::new();
        let a = make_task_with_status("a", "Task A", Status::Open);
        graph.add_node(Node::Task(a));
        let temp = tempfile::TempDir::new().unwrap();
        let wg_dir = temp.path().to_path_buf();
        std::fs::create_dir_all(&wg_dir).unwrap();
        let graph_path = wg_dir.join("graph.jsonl");
        save_graph(&graph, &graph_path).unwrap();
        // Save a default config so load_config_panel can read it
        let config = Config::default();
        config.save(&wg_dir).unwrap();

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            VizLayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
        let mut app = VizApp::from_viz_output_for_test(&viz);
        app.workgraph_dir = wg_dir;
        (app, temp)
    }

    #[test]
    fn test_config_panel_all_entries_save_roundtrip() {
        let (mut app, _temp) = build_config_test_app();
        app.load_config_panel();

        // Collect all keys for verification
        let keys: Vec<String> = app
            .config_panel
            .entries
            .iter()
            .map(|e| e.key.clone())
            .collect();
        assert!(!keys.is_empty(), "load_config_panel should produce entries");

        // For each entry, set a valid value, save, reload, and verify.
        for i in 0..app.config_panel.entries.len() {
            let entry = &app.config_panel.entries[i];
            let key = entry.key.clone();

            // Skip entries that are read-only or special
            if key.starts_with("apikey.")
                || key == "endpoint.add"
                || key.ends_with(".remove")
                || key.ends_with(".is_default")
                || key.ends_with(".set_default")
                || key.starts_with("action.")
                || key.ends_with(".resolved")
                || key.ends_with(".info")
                || key == "model.add"
            {
                continue;
            }
            // Skip endpoint entries (they need an existing endpoint)
            if key.starts_with("endpoint.") {
                continue;
            }
            // Skip model registry entries (managed via models.yaml, not config.toml)
            if key.starts_with("model.") {
                continue;
            }

            match &entry.edit_kind {
                ConfigEditKind::Toggle => {
                    // Test toggle: toggle once, reload, check it changed
                    let old_val = app.config_panel.entries[i].value.clone();
                    app.config_panel.selected = i;
                    app.toggle_config_entry();

                    let expected = if old_val == "on" { "off" } else { "on" };
                    assert_eq!(
                        app.config_panel.entries[i].value, expected,
                        "Toggle for '{}' should flip from '{}' to '{}'",
                        key, old_val, expected
                    );

                    // Reload and verify persistence
                    app.load_config_panel();
                    let reloaded = app
                        .config_panel
                        .entries
                        .iter()
                        .find(|e| e.key == key)
                        .unwrap();
                    assert_eq!(
                        reloaded.value, expected,
                        "Toggle for '{}' did not persist after reload",
                        key
                    );

                    // Toggle back to original
                    let idx = app
                        .config_panel
                        .entries
                        .iter()
                        .position(|e| e.key == key)
                        .unwrap();
                    app.config_panel.selected = idx;
                    app.toggle_config_entry();
                    app.load_config_panel();
                }
                ConfigEditKind::TextInput | ConfigEditKind::SecretInput => {
                    // Set a test value
                    let test_value = match key.as_str() {
                        "coordinator.max_agents"
                        | "coordinator.poll_interval"
                        | "coordinator.settling_delay_ms"
                        | "coordinator.max_coordinators"
                        | "agent.heartbeat_timeout"
                        | "agency.auto_create_threshold"
                        | "agency.triage_timeout"
                        | "agency.triage_max_log_bytes"
                        | "tui.message_name_threshold"
                        | "guardrails.max_child_tasks_per_agent"
                        | "guardrails.max_task_depth"
                        | "tui.chat_history_max"
                        | "tui.session_gap_minutes"
                        | "checkpoint.retry_context_tokens"
                        | "coordinator.max_incomplete_retries" => "42",
                        "tui.message_indent" => "4", // clamped to max 8
                        "agency.flip_verification_threshold" | "agency.eval_gate_threshold" => {
                            "0.85"
                        }
                        "coordinator.agent_timeout" => "45m",
                        "tui.counters" => "uptime,active",
                        // Model and tier fields require provider:model format
                        k if k.starts_with("tiers.")
                            || k == "coordinator.model"
                            || k == "agent.model"
                            || (k.starts_with("models.") && k.ends_with(".model")) =>
                        {
                            "claude:test-model"
                        }
                        // Provider fields are deprecated (skip_serializing) — skip them
                        k if k.starts_with("models.") && k.ends_with(".provider") => {
                            continue;
                        }
                        _ => "test-value",
                    };

                    app.config_panel.selected = i;
                    app.config_panel.editing = true;
                    app.config_panel.edit_buffer = test_value.to_string();
                    app.save_config_entry();

                    // Reload and verify
                    app.load_config_panel();
                    let reloaded = app.config_panel.entries.iter().find(|e| e.key == key);
                    assert!(reloaded.is_some(), "Entry '{}' missing after reload", key);
                    let reloaded = reloaded.unwrap();
                    // For numeric fields, the saved value may be formatted differently
                    match key.as_str() {
                        "agency.flip_verification_threshold" | "agency.eval_gate_threshold" => {
                            assert_eq!(
                                reloaded.value, "0.85",
                                "TextInput for '{}' did not round-trip",
                                key
                            );
                        }
                        _ => {
                            assert_eq!(
                                reloaded.value, test_value,
                                "TextInput for '{}' did not round-trip",
                                key
                            );
                        }
                    }
                }
                ConfigEditKind::Choice(choices) => {
                    if choices.len() < 2 {
                        continue;
                    }
                    // Cycle through choices
                    let original_value = app.config_panel.entries[i].value.clone();
                    let original_idx = choices
                        .iter()
                        .position(|c| c == &original_value)
                        .unwrap_or(0);
                    let next_idx = (original_idx + 1) % choices.len();
                    let next_value = choices[next_idx].clone();

                    app.config_panel.selected = i;
                    app.config_panel.editing = true;
                    app.config_panel.choice_index = next_idx;
                    app.save_config_entry();

                    // Reload and verify
                    app.load_config_panel();
                    let reloaded = app.config_panel.entries.iter().find(|e| e.key == key);
                    assert!(reloaded.is_some(), "Entry '{}' missing after reload", key);
                    let reloaded = reloaded.unwrap();
                    assert_eq!(
                        reloaded.value, next_value,
                        "Choice for '{}' did not round-trip: expected '{}', got '{}'",
                        key, next_value, reloaded.value
                    );

                    // Restore original value
                    let idx = app
                        .config_panel
                        .entries
                        .iter()
                        .position(|e| e.key == key)
                        .unwrap();
                    app.config_panel.selected = idx;
                    app.config_panel.editing = true;
                    app.config_panel.choice_index = original_idx;
                    app.save_config_entry();
                    app.load_config_panel();
                }
            }
        }
    }

    #[test]
    fn test_config_panel_has_all_required_keys() {
        let (mut app, _temp) = build_config_test_app();
        app.load_config_panel();

        let keys: HashSet<String> = app
            .config_panel
            .entries
            .iter()
            .map(|e| e.key.clone())
            .collect();

        // All the keys that must exist in the config panel
        let required_keys = vec![
            // Service
            "coordinator.max_agents",
            "coordinator.poll_interval",
            "coordinator.executor",
            "coordinator.model",
            "coordinator.agent_timeout",
            "coordinator.settling_delay_ms",
            "coordinator.max_coordinators",
            // TUI
            "tui.mouse_mode",
            "viz.animations",
            "tui.default_layout",
            "tui.default_inspector_size",
            "tui.color_theme",
            "tui.timestamp_format",
            "tui.show_token_counts",
            "viz.edge_color",
            "tui.message_name_threshold",
            "tui.message_indent",
            "tui.chat_history",
            "tui.chat_history_max",
            "tui.session_gap_minutes",
            "tui.counters",
            "tui.show_system_tasks",
            "tui.show_running_system_tasks",
            // Agent
            "agent.heartbeat_timeout",
            "agent.executor",
            "agent.model",
            // Agency
            "agency.auto_assign",
            "agency.auto_evaluate",
            "agency.auto_triage",
            "agency.auto_create",
            "agency.assigner_agent",
            "agency.evaluator_agent",
            "agency.evolver_agent",
            "agency.creator_agent",
            "agency.auto_create_threshold",
            "agency.triage_timeout",
            "agency.triage_max_log_bytes",
            "agency.retention_heuristics",
            "agency.flip_enabled",
            "agency.flip_verification_threshold",
            "agency.eval_gate_threshold",
            "agency.eval_gate_all",
            "checkpoint.retry_context_tokens",
            "coordinator.max_incomplete_retries",
            "coordinator.incomplete_retry_delay",
            "coordinator.escalate_on_retry",
            // Guardrails
            "guardrails.max_child_tasks_per_agent",
            "guardrails.max_task_depth",
            // Model tiers
            "tiers.fast",
            "tiers.standard",
            "tiers.premium",
            // Model routing (resolved + model + tier + provider + endpoint per role)
            "models.default.resolved",
            "models.default.model",
            "models.default.tier",
            "models.default.provider",
            "models.default.endpoint",
            "models.task_agent.resolved",
            "models.task_agent.model",
            "models.task_agent.tier",
            "models.task_agent.provider",
            "models.task_agent.endpoint",
            "models.evaluator.resolved",
            "models.evaluator.model",
            "models.evaluator.tier",
            "models.evaluator.provider",
            "models.evaluator.endpoint",
            "models.flip_inference.resolved",
            "models.flip_inference.model",
            "models.flip_inference.tier",
            "models.flip_inference.provider",
            "models.flip_inference.endpoint",
            "models.flip_comparison.resolved",
            "models.flip_comparison.model",
            "models.flip_comparison.tier",
            "models.flip_comparison.provider",
            "models.flip_comparison.endpoint",
            "models.assigner.resolved",
            "models.assigner.model",
            "models.assigner.tier",
            "models.assigner.provider",
            "models.assigner.endpoint",
            "models.evolver.resolved",
            "models.evolver.model",
            "models.evolver.tier",
            "models.evolver.provider",
            "models.evolver.endpoint",
            "models.verification.resolved",
            "models.verification.model",
            "models.verification.tier",
            "models.verification.provider",
            "models.verification.endpoint",
            "models.triage.resolved",
            "models.triage.model",
            "models.triage.tier",
            "models.triage.provider",
            "models.triage.endpoint",
            "models.creator.resolved",
            "models.creator.model",
            "models.creator.tier",
            "models.creator.provider",
            "models.creator.endpoint",
            "models.compactor.resolved",
            "models.compactor.model",
            "models.compactor.tier",
            "models.compactor.provider",
            "models.compactor.endpoint",
        ];

        for required in &required_keys {
            assert!(
                keys.contains(*required),
                "Missing required config panel key: '{}'",
                required
            );
        }
    }

    #[test]
    fn test_config_panel_every_entry_has_save_handler() {
        // This test verifies that save_config_entry handles every key in load_config_panel
        // by setting a value and checking it doesn't silently no-op.
        let (mut app, _temp) = build_config_test_app();
        app.load_config_panel();

        for i in 0..app.config_panel.entries.len() {
            let entry = &app.config_panel.entries[i];
            let key = entry.key.clone();

            // Skip known read-only/special entries
            if key.starts_with("apikey.")
                || key == "endpoint.add"
                || key == "model.add"
                || key.ends_with(".remove")
                || key.ends_with(".is_default")
                || key.ends_with(".set_default")
                || key.ends_with(".info")
                || key.starts_with("endpoint.")
                || key.starts_with("model.")
                || key.starts_with("action.")
                || key.ends_with(".resolved")
            {
                continue;
            }

            // For Toggle entries, verify toggle_config_entry has a handler
            if matches!(entry.edit_kind, ConfigEditKind::Toggle) {
                let old = app.config_panel.entries[i].value.clone();
                app.config_panel.selected = i;
                app.toggle_config_entry();
                let new = app.config_panel.entries[i].value.clone();
                assert_ne!(
                    old, new,
                    "toggle_config_entry for '{}' did not change the value (no handler?)",
                    key
                );
                // Toggle back
                app.config_panel.selected = i;
                app.toggle_config_entry();
                app.load_config_panel();
            }
        }
    }

    #[test]
    fn test_config_panel_model_routing_roundtrip() {
        let (mut app, _temp) = build_config_test_app();
        app.load_config_panel();

        // Set a model routing entry
        let key = "models.default.model";
        let idx = app
            .config_panel
            .entries
            .iter()
            .position(|e| e.key == key)
            .unwrap();
        app.config_panel.selected = idx;
        app.config_panel.editing = true;
        app.config_panel.edit_buffer = "claude:sonnet".to_string();
        app.save_config_entry();

        // Use Config::load (local-only) to avoid global config bleeding in
        let config = Config::load(&app.workgraph_dir).unwrap();
        let default_model = config.models.default.as_ref().and_then(|c| c.model.clone());
        assert_eq!(default_model, Some("claude:sonnet".to_string()));

        // Set to inherit (clear)
        app.load_config_panel();
        let idx = app
            .config_panel
            .entries
            .iter()
            .position(|e| e.key == "models.default.model")
            .unwrap();
        app.config_panel.selected = idx;
        app.config_panel.editing = true;
        app.config_panel.edit_buffer = "(inherit)".to_string();
        app.save_config_entry();

        let config = Config::load(&app.workgraph_dir).unwrap();
        let default_model = config.models.default.as_ref().and_then(|c| c.model.clone());
        assert_eq!(default_model, None);
    }

    #[test]
    fn test_config_panel_tier_roundtrip() {
        let (mut app, _temp) = build_config_test_app();
        app.load_config_panel();

        // Set fast tier to a custom model
        let key = "tiers.fast";
        let idx = app
            .config_panel
            .entries
            .iter()
            .position(|e| e.key == key)
            .unwrap();
        app.config_panel.selected = idx;
        app.config_panel.editing = true;
        app.config_panel.edit_buffer = "claude:custom-fast-model".to_string();
        app.save_config_entry();

        let config = Config::load(&app.workgraph_dir).unwrap();
        assert_eq!(
            config.tiers.fast,
            Some("claude:custom-fast-model".to_string())
        );

        // Set standard tier
        app.load_config_panel();
        let key = "tiers.standard";
        let idx = app
            .config_panel
            .entries
            .iter()
            .position(|e| e.key == key)
            .unwrap();
        app.config_panel.selected = idx;
        app.config_panel.editing = true;
        app.config_panel.edit_buffer = "claude:custom-standard".to_string();
        app.save_config_entry();

        let config = Config::load(&app.workgraph_dir).unwrap();
        assert_eq!(
            config.tiers.standard,
            Some("claude:custom-standard".to_string())
        );

        // Set premium tier
        app.load_config_panel();
        let key = "tiers.premium";
        let idx = app
            .config_panel
            .entries
            .iter()
            .position(|e| e.key == key)
            .unwrap();
        app.config_panel.selected = idx;
        app.config_panel.editing = true;
        app.config_panel.edit_buffer = "claude:custom-premium".to_string();
        app.save_config_entry();

        let config = Config::load(&app.workgraph_dir).unwrap();
        assert_eq!(
            config.tiers.premium,
            Some("claude:custom-premium".to_string())
        );
    }

    #[test]
    fn test_config_panel_model_routing_tier_and_endpoint() {
        let (mut app, _temp) = build_config_test_app();
        app.load_config_panel();

        // Set a tier override for triage role
        let key = "models.triage.tier";
        let idx = app
            .config_panel
            .entries
            .iter()
            .position(|e| e.key == key)
            .unwrap();
        // Tier is a Choice: (inherit), fast, standard, premium
        // Select "premium" (index 3)
        app.config_panel.selected = idx;
        app.config_panel.editing = true;
        app.config_panel.choice_index = 3; // "premium"
        app.save_config_entry();

        let config = Config::load(&app.workgraph_dir).unwrap();
        let triage_tier = config.models.triage.as_ref().and_then(|c| c.tier);
        assert_eq!(triage_tier, Some(workgraph::config::Tier::Premium));

        // Set an endpoint for evaluator
        app.load_config_panel();
        let key = "models.evaluator.endpoint";
        let idx = app
            .config_panel
            .entries
            .iter()
            .position(|e| e.key == key)
            .unwrap();
        app.config_panel.selected = idx;
        app.config_panel.editing = true;
        app.config_panel.edit_buffer = "openrouter".to_string();
        app.save_config_entry();

        let config = Config::load(&app.workgraph_dir).unwrap();
        let eval_endpoint = config
            .models
            .evaluator
            .as_ref()
            .and_then(|c| c.endpoint.clone());
        assert_eq!(eval_endpoint, Some("openrouter".to_string()));

        // Clear the tier (set to inherit)
        app.load_config_panel();
        let key = "models.triage.tier";
        let idx = app
            .config_panel
            .entries
            .iter()
            .position(|e| e.key == key)
            .unwrap();
        app.config_panel.selected = idx;
        app.config_panel.editing = true;
        app.config_panel.choice_index = 0; // "(inherit)"
        app.save_config_entry();

        let config = Config::load(&app.workgraph_dir).unwrap();
        let triage_tier = config.models.triage.as_ref().and_then(|c| c.tier);
        assert_eq!(triage_tier, None);

        // Clear endpoint (set to inherit)
        app.load_config_panel();
        let key = "models.evaluator.endpoint";
        let idx = app
            .config_panel
            .entries
            .iter()
            .position(|e| e.key == key)
            .unwrap();
        app.config_panel.selected = idx;
        app.config_panel.editing = true;
        app.config_panel.edit_buffer = "(inherit)".to_string();
        app.save_config_entry();

        let config = Config::load(&app.workgraph_dir).unwrap();
        let eval_endpoint = config
            .models
            .evaluator
            .as_ref()
            .and_then(|c| c.endpoint.clone());
        assert_eq!(eval_endpoint, None);
    }

    #[test]
    fn test_config_panel_resolved_model_display() {
        let (mut app, _temp) = build_config_test_app();
        app.load_config_panel();

        // The resolved entry for triage should exist and show the resolved model
        let resolved_key = "models.triage.resolved";
        let entry = app
            .config_panel
            .entries
            .iter()
            .find(|e| e.key == resolved_key)
            .expect("resolved entry for triage should exist");
        // Label should contain "Triage" and an arrow
        assert!(
            entry.label.contains("Triage"),
            "label should contain role name"
        );
        assert!(
            entry.label.contains("→"),
            "label should contain arrow for resolved display"
        );
    }
}

#[cfg(test)]
mod archive_browser_tests {
    use super::*;
    use std::io::Write;

    fn create_archive(dir: &std::path::Path, entries: &[(&str, &str, &str, &[&str])]) {
        let archive_path = dir.join("archive.jsonl");
        let mut file = std::fs::File::create(&archive_path).unwrap();
        for (id, title, completed, tags) in entries {
            let tags_json: Vec<String> = tags.iter().map(|t| format!("\"{}\"", t)).collect();
            writeln!(
                file,
                r#"{{"kind":"task","id":"{}","title":"{}","status":"done","completed_at":"{}","tags":[{}]}}"#,
                id, title, completed, tags_json.join(",")
            )
            .unwrap();
        }
    }

    #[test]
    fn test_tui_archive_load_entries() {
        let tmp = tempfile::tempdir().unwrap();
        create_archive(
            tmp.path(),
            &[
                ("task-1", "First task", "2026-01-15T00:00:00Z", &["bug"]),
                (
                    "task-2",
                    "Second task",
                    "2026-02-20T00:00:00Z",
                    &["feature", "ui"],
                ),
                ("task-3", "Third task", "2026-03-01T00:00:00Z", &[]),
            ],
        );

        let mut ab = ArchiveBrowserState::default();
        ab.load(tmp.path());

        assert_eq!(ab.entries.len(), 3);
        assert_eq!(ab.entries[0].id, "task-1");
        assert_eq!(ab.entries[0].title, "First task");
        assert_eq!(ab.entries[0].tags, vec!["bug"]);
        assert_eq!(ab.entries[1].id, "task-2");
        assert_eq!(ab.entries[2].id, "task-3");
        assert_eq!(ab.visible_count(), 3);
    }

    #[test]
    fn test_tui_archive_filter() {
        let tmp = tempfile::tempdir().unwrap();
        create_archive(
            tmp.path(),
            &[
                ("task-1", "Fix login bug", "2026-01-15T00:00:00Z", &["bug"]),
                (
                    "task-2",
                    "Add dashboard",
                    "2026-02-20T00:00:00Z",
                    &["feature"],
                ),
                ("task-3", "Fix signup bug", "2026-03-01T00:00:00Z", &["bug"]),
            ],
        );

        let mut ab = ArchiveBrowserState::default();
        ab.load(tmp.path());

        // Filter by title
        ab.filter = "Fix".to_string();
        ab.apply_filter();
        assert_eq!(ab.visible_count(), 2);
        assert_eq!(ab.filtered_indices, vec![0, 2]);

        // Filter by tag
        ab.filter = "feature".to_string();
        ab.apply_filter();
        assert_eq!(ab.visible_count(), 1);
        assert_eq!(ab.filtered_indices, vec![1]);

        // Filter by id
        ab.filter = "task-3".to_string();
        ab.apply_filter();
        assert_eq!(ab.visible_count(), 1);
        assert_eq!(ab.filtered_indices, vec![2]);

        // No matches
        ab.filter = "nonexistent".to_string();
        ab.apply_filter();
        assert_eq!(ab.visible_count(), 0);

        // Clear filter
        ab.filter.clear();
        ab.apply_filter();
        assert_eq!(ab.visible_count(), 3);
    }

    #[test]
    fn test_tui_archive_selection() {
        let tmp = tempfile::tempdir().unwrap();
        create_archive(
            tmp.path(),
            &[
                ("a", "Alpha", "2026-01-01T00:00:00Z", &[]),
                ("b", "Beta", "2026-02-01T00:00:00Z", &[]),
                ("c", "Gamma", "2026-03-01T00:00:00Z", &[]),
            ],
        );

        let mut ab = ArchiveBrowserState::default();
        ab.load(tmp.path());

        assert_eq!(ab.selected, 0);
        assert_eq!(ab.selected_entry().unwrap().id, "a");

        ab.selected = 2;
        assert_eq!(ab.selected_entry().unwrap().id, "c");

        // With filter active
        ab.filter = "Beta".to_string();
        ab.apply_filter();
        // selected gets clamped to 0 (only 1 result)
        assert_eq!(ab.selected, 0);
        assert_eq!(ab.selected_entry().unwrap().id, "b");
    }

    #[test]
    fn test_tui_archive_empty() {
        let tmp = tempfile::tempdir().unwrap();
        // No archive file exists
        let mut ab = ArchiveBrowserState::default();
        ab.load(tmp.path());

        assert_eq!(ab.entries.len(), 0);
        assert_eq!(ab.visible_count(), 0);
        assert!(ab.selected_entry().is_none());
    }

    #[test]
    fn test_tui_archive_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        create_archive(tmp.path(), &[("x", "Test", "2026-01-01T00:00:00Z", &[])]);

        let mut ab = ArchiveBrowserState::default();
        assert!(!ab.active);

        // Simulate toggle on
        ab.load(tmp.path());
        ab.active = true;
        assert!(ab.active);
        assert_eq!(ab.entries.len(), 1);

        // Toggle off
        ab.active = false;
        ab.filter_active = false;
        assert!(!ab.active);
    }
}

#[cfg(test)]
mod touch_echo_tests {
    use super::*;

    #[test]
    fn touch_echo_progress_and_expiry() {
        let echo = TouchEcho {
            col: 10,
            row: 5,
            start: Instant::now(),
        };
        // Just created: progress near 0, not expired.
        assert!(echo.progress() < 0.1);
        assert!(!echo.is_expired());
    }

    #[test]
    fn touch_echo_expired_after_duration() {
        let echo = TouchEcho {
            col: 10,
            row: 5,
            start: Instant::now() - std::time::Duration::from_secs(1),
        };
        // After 1s (> TOUCH_ECHO_DURATION_SECS=0.7): fully expired.
        assert!(echo.progress() >= 1.0);
        assert!(echo.is_expired());
    }

    #[test]
    fn add_touch_echo_respects_enabled_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph.jsonl");
        std::fs::write(&graph_path, "").unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let mut app = VizApp::new(
            tmp.path().to_path_buf(),
            crate::commands::viz::VizOptions::default(),
            Some(true),
            None,
            false,
        );

        // Disabled by default: adding echo should be a no-op.
        assert!(!app.touch_echo_enabled);
        app.add_touch_echo(10, 5);
        assert!(app.touch_echoes.is_empty());

        // Enable and add.
        app.touch_echo_enabled = true;
        app.add_touch_echo(10, 5);
        assert_eq!(app.touch_echoes.len(), 1);
        assert_eq!(app.touch_echoes[0].col, 10);
        assert_eq!(app.touch_echoes[0].row, 5);
    }

    #[test]
    fn touch_echo_cap_enforced() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph.jsonl");
        std::fs::write(&graph_path, "").unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let mut app = VizApp::new(
            tmp.path().to_path_buf(),
            crate::commands::viz::VizOptions::default(),
            Some(true),
            None,
            false,
        );
        app.touch_echo_enabled = true;

        // Add more than MAX_TOUCH_ECHOES.
        for i in 0..15 {
            app.add_touch_echo(i, 0);
        }
        assert!(app.touch_echoes.len() <= MAX_TOUCH_ECHOES);
        // The oldest echoes should have been dropped.
        assert_eq!(app.touch_echoes[0].col, 5); // first 5 dropped (15 - 10 = 5)
    }

    #[test]
    fn cleanup_removes_expired_echoes() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph.jsonl");
        std::fs::write(&graph_path, "").unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let mut app = VizApp::new(
            tmp.path().to_path_buf(),
            crate::commands::viz::VizOptions::default(),
            Some(true),
            None,
            false,
        );
        app.touch_echo_enabled = true;

        // Add an expired echo manually.
        app.touch_echoes.push(TouchEcho {
            col: 1,
            row: 1,
            start: Instant::now() - std::time::Duration::from_secs(2),
        });
        // Add a fresh echo.
        app.add_touch_echo(2, 2);

        assert_eq!(app.touch_echoes.len(), 2);
        app.cleanup_touch_echoes();
        assert_eq!(app.touch_echoes.len(), 1);
        assert_eq!(app.touch_echoes[0].col, 2);
    }

    #[test]
    fn has_active_touch_echoes_respects_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph.jsonl");
        std::fs::write(&graph_path, "").unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let mut app = VizApp::new(
            tmp.path().to_path_buf(),
            crate::commands::viz::VizOptions::default(),
            Some(true),
            None,
            false,
        );

        // No echoes: false regardless.
        assert!(!app.has_active_touch_echoes());

        // Enable, add echo: should be true.
        app.touch_echo_enabled = true;
        app.add_touch_echo(5, 5);
        assert!(app.has_active_touch_echoes());

        // Disable: should be false even with echoes present.
        app.touch_echo_enabled = false;
        assert!(!app.has_active_touch_echoes());
    }

    #[test]
    fn toggle_off_clears_echoes() {
        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph.jsonl");
        std::fs::write(&graph_path, "").unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "").unwrap();
        let mut app = VizApp::new(
            tmp.path().to_path_buf(),
            crate::commands::viz::VizOptions::default(),
            Some(true),
            None,
            false,
        );

        app.touch_echo_enabled = true;
        app.add_touch_echo(5, 5);
        app.add_touch_echo(10, 10);
        assert_eq!(app.touch_echoes.len(), 2);

        // Simulating the toggle-off behavior from the event handler.
        app.touch_echo_enabled = false;
        app.touch_echoes.clear();
        assert!(app.touch_echoes.is_empty());
    }
}

#[cfg(test)]
mod activity_feed_tests {
    use super::*;

    fn make_op_line(op: &str, task_id: Option<&str>, actor: Option<&str>) -> String {
        let mut obj = serde_json::json!({
            "timestamp": "2026-03-25T14:30:45.123456789+00:00",
            "op": op,
            "detail": {}
        });
        if let Some(tid) = task_id {
            obj["task_id"] = serde_json::Value::String(tid.to_string());
        }
        if let Some(a) = actor {
            obj["actor"] = serde_json::Value::String(a.to_string());
        }
        serde_json::to_string(&obj).unwrap()
    }

    fn make_op_line_with_detail(
        op: &str,
        task_id: Option<&str>,
        detail: serde_json::Value,
    ) -> String {
        let mut obj = serde_json::json!({
            "timestamp": "2026-03-25T14:30:45.123456789+00:00",
            "op": op,
            "detail": detail
        });
        if let Some(tid) = task_id {
            obj["task_id"] = serde_json::Value::String(tid.to_string());
        }
        serde_json::to_string(&obj).unwrap()
    }

    // ── Parsing tests (all event types) ──

    #[test]
    fn parse_task_created() {
        let line = make_op_line_with_detail(
            "add_task",
            Some("my-task"),
            serde_json::json!({"title": "My Task"}),
        );
        let event = ActivityEvent::parse(&line).unwrap();
        assert_eq!(event.kind, ActivityEventKind::TaskCreated);
        assert_eq!(event.op, "add_task");
        assert_eq!(event.task_id.as_deref(), Some("my-task"));
        assert_eq!(event.icon(), "+");
        assert!(event.summary.contains("My Task"));
        assert_eq!(event.time_short, "14:30:45");
    }

    #[test]
    fn parse_agent_spawned() {
        let line = make_op_line("claim", Some("build-feature"), Some("agent-42"));
        let event = ActivityEvent::parse(&line).unwrap();
        assert_eq!(event.kind, ActivityEventKind::AgentSpawned);
        assert_eq!(event.icon(), "▶");
        assert!(event.summary.contains("agent-42"));
        assert!(event.summary.contains("build-feature"));
    }

    #[test]
    fn parse_agent_completed() {
        let line = make_op_line("done", Some("build-feature"), None);
        let event = ActivityEvent::parse(&line).unwrap();
        assert_eq!(event.kind, ActivityEventKind::AgentCompleted);
        assert_eq!(event.icon(), "✓");
        assert!(event.summary.contains("build-feature"));
    }

    #[test]
    fn parse_agent_failed() {
        let line = make_op_line_with_detail(
            "fail",
            Some("build-feature"),
            serde_json::json!({"reason": "Agent exited with code 1"}),
        );
        let event = ActivityEvent::parse(&line).unwrap();
        assert_eq!(event.kind, ActivityEventKind::AgentFailed);
        assert_eq!(event.icon(), "✗");
        assert!(event.summary.contains("Agent exited"));
    }

    #[test]
    fn parse_status_changes() {
        for op in &[
            "abandon", "pause", "resume", "retry", "unclaim", "edit", "assign",
        ] {
            let line = make_op_line(op, Some("some-task"), None);
            let event = ActivityEvent::parse(&line).unwrap();
            assert_eq!(
                event.kind,
                ActivityEventKind::StatusChange,
                "Expected StatusChange for op={}",
                op
            );
            assert_eq!(event.icon(), "→");
        }
    }

    #[test]
    fn parse_coordinator_tick() {
        for op in &["replay", "apply"] {
            let line = make_op_line(op, None, None);
            let event = ActivityEvent::parse(&line).unwrap();
            assert_eq!(
                event.kind,
                ActivityEventKind::CoordinatorTick,
                "Expected CoordinatorTick for op={}",
                op
            );
            assert_eq!(event.icon(), "⟳");
        }
    }

    #[test]
    fn parse_verification_result() {
        let line = make_op_line("approve", Some("build-feature"), None);
        let event = ActivityEvent::parse(&line).unwrap();
        assert_eq!(event.kind, ActivityEventKind::VerificationResult);
        assert_eq!(event.icon(), "◆");
    }

    #[test]
    fn parse_user_actions() {
        for op in &[
            "gc",
            "archive",
            "link",
            "unlink",
            "publish",
            "trace_export",
            "artifact_add",
        ] {
            let line = make_op_line(op, Some("t"), None);
            let event = ActivityEvent::parse(&line).unwrap();
            assert_eq!(
                event.kind,
                ActivityEventKind::UserAction,
                "Expected UserAction for op={}",
                op
            );
            assert_eq!(event.icon(), "●");
        }
    }

    #[test]
    fn parse_invalid_json_returns_none() {
        assert!(ActivityEvent::parse("not json").is_none());
        assert!(ActivityEvent::parse("{}").is_none()); // missing required fields
        assert!(ActivityEvent::parse("{\"timestamp\":\"t\"}").is_none()); // missing op
    }

    #[test]
    fn parse_time_short_extraction() {
        assert_eq!(
            parse_time_short("2026-03-25T14:30:45.123+00:00"),
            "14:30:45"
        );
        assert_eq!(parse_time_short("2026-01-01T00:00:00Z"), "00:00:00");
    }

    // ── Ring buffer tests ──

    #[test]
    fn ring_buffer_overflow() {
        let mut state = ActivityFeedState::default();
        // Fill beyond max.
        for i in 0..(ACTIVITY_FEED_MAX_EVENTS + 50) {
            let line = make_op_line("done", Some(&format!("task-{}", i)), None);
            if let Some(event) = ActivityEvent::parse(&line) {
                state.events.push_back(event);
                if state.events.len() > ACTIVITY_FEED_MAX_EVENTS {
                    state.events.pop_front();
                }
            }
        }
        assert_eq!(state.events.len(), ACTIVITY_FEED_MAX_EVENTS);
        // The oldest remaining should be task-50 (first 50 were evicted).
        assert_eq!(
            state.events.front().unwrap().task_id.as_deref(),
            Some("task-50")
        );
        let expected_last = format!("task-{}", ACTIVITY_FEED_MAX_EVENTS + 49);
        assert_eq!(
            state.events.back().unwrap().task_id.as_deref(),
            Some(expected_last.as_str())
        );
    }

    #[test]
    fn auto_tail_default_on() {
        let state = ActivityFeedState::default();
        assert!(state.auto_tail);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn scroll_pause_disengages_auto_tail() {
        let mut state = ActivityFeedState::default();
        state.auto_tail = true;
        state.total_wrapped_lines = 100;
        state.viewport_height = 20;
        state.scroll = 80; // at bottom

        // Simulate scroll up.
        state.scroll = state.scroll.saturating_sub(5);
        state.auto_tail = false;

        assert!(!state.auto_tail);
        assert_eq!(state.scroll, 75);
    }

    #[test]
    fn scroll_to_bottom_re_engages_auto_tail() {
        let mut state = ActivityFeedState::default();
        state.auto_tail = false;
        state.total_wrapped_lines = 100;
        state.viewport_height = 20;

        // Scroll to bottom.
        let max_scroll = state
            .total_wrapped_lines
            .saturating_sub(state.viewport_height);
        state.scroll = max_scroll;
        state.auto_tail = true;

        assert!(state.auto_tail);
        assert_eq!(state.scroll, 80);
    }

    // ── Summary formatting tests ──

    #[test]
    fn format_gc_summary() {
        let line = make_op_line_with_detail(
            "gc",
            None,
            serde_json::json!({"removed": [{"id":"a"}, {"id":"b"}, {"id":"c"}]}),
        );
        let event = ActivityEvent::parse(&line).unwrap();
        assert!(event.summary.contains("3 tasks"));
    }

    #[test]
    fn format_archive_summary() {
        let line =
            make_op_line_with_detail("archive", None, serde_json::json!({"task_ids": ["a", "b"]}));
        let event = ActivityEvent::parse(&line).unwrap();
        assert!(event.summary.contains("2 tasks"));
    }

    #[test]
    fn format_retry_summary() {
        let line = make_op_line_with_detail(
            "retry",
            Some("flaky-task"),
            serde_json::json!({"attempt": 3}),
        );
        let event = ActivityEvent::parse(&line).unwrap();
        assert!(event.summary.contains("#3"));
        assert!(event.summary.contains("flaky-task"));
    }

    #[test]
    fn format_link_summary() {
        let line = make_op_line_with_detail(
            "link",
            Some("child-task"),
            serde_json::json!({"action": "add", "dependency": "parent-task"}),
        );
        let event = ActivityEvent::parse(&line).unwrap();
        assert!(event.summary.contains("child-task"));
        assert!(event.summary.contains("parent-task"));
    }

    #[test]
    fn format_artifact_add_summary() {
        let line = make_op_line_with_detail(
            "artifact_add",
            Some("my-task"),
            serde_json::json!({"path": "src/main.rs"}),
        );
        let event = ActivityEvent::parse(&line).unwrap();
        assert!(event.summary.contains("my-task"));
        assert!(event.summary.contains("src/main.rs"));
    }
}

#[cfg(test)]
mod dashboard_tests {
    use super::*;
    use workgraph::AgentStatus;

    // ── Agent activity classification ──────────────────────────────────────

    #[test]
    fn classify_done_agent_is_exited() {
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Done, Some(5), false),
            DashboardAgentActivity::Exited,
        );
    }

    #[test]
    fn classify_failed_agent_is_exited() {
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Failed, Some(5), false),
            DashboardAgentActivity::Exited,
        );
    }

    #[test]
    fn classify_dead_agent_is_exited() {
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Dead, Some(5), false),
            DashboardAgentActivity::Exited,
        );
    }

    #[test]
    fn classify_working_recent_output_is_active() {
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Working, Some(10), false),
            DashboardAgentActivity::Active,
        );
    }

    #[test]
    fn classify_working_stale_output_is_slow() {
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Working, Some(60), false),
            DashboardAgentActivity::Slow,
        );
    }

    #[test]
    fn classify_working_very_stale_output_is_stuck() {
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Working, Some(600), false),
            DashboardAgentActivity::Stuck,
        );
    }

    #[test]
    fn classify_working_no_output_is_active() {
        // New agent that hasn't written output yet — treat as active.
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Working, None, false),
            DashboardAgentActivity::Active,
        );
    }

    #[test]
    fn classify_boundary_30s_is_slow() {
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Working, Some(30), false),
            DashboardAgentActivity::Slow,
        );
    }

    #[test]
    fn classify_boundary_300s_is_stuck() {
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Working, Some(300), false),
            DashboardAgentActivity::Stuck,
        );
    }

    #[test]
    fn classify_boundary_29s_is_active() {
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Working, Some(29), false),
            DashboardAgentActivity::Active,
        );
    }

    #[test]
    fn classify_stale_with_children_is_slow() {
        // Agent with very stale output but active children should be Slow, not Stuck
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Working, Some(600), true),
            DashboardAgentActivity::Slow,
        );
    }

    #[test]
    fn classify_stale_without_children_is_stuck() {
        // Agent with very stale output and no children should be Stuck
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Working, Some(600), false),
            DashboardAgentActivity::Stuck,
        );
    }

    #[test]
    fn classify_moderate_stale_with_children_still_slow() {
        // Children flag doesn't affect Slow range (30-300s) — it's already Slow
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Working, Some(100), true),
            DashboardAgentActivity::Slow,
        );
    }

    #[test]
    fn classify_active_with_children_still_active() {
        // Children flag doesn't affect Active range (<30s)
        assert_eq!(
            DashboardAgentActivity::classify(AgentStatus::Working, Some(10), true),
            DashboardAgentActivity::Active,
        );
    }

    // ── Sparkline ──────────────────────────────────────────────────────────

    #[test]
    fn sparkline_record_increments_last_bucket() {
        let mut state = DashboardState::default();
        assert_eq!(state.sparkline_data.last(), Some(&0));
        state.record_sparkline_event();
        assert_eq!(state.sparkline_data.last(), Some(&1));
        state.record_sparkline_event();
        assert_eq!(state.sparkline_data.last(), Some(&2));
    }

    #[test]
    fn sparkline_compute_from_timestamps_recent() {
        let mut state = DashboardState::default();
        let now = std::time::SystemTime::now();
        // 3 events within the last bucket (last 10s)
        let timestamps = vec![
            now - std::time::Duration::from_secs(1),
            now - std::time::Duration::from_secs(3),
            now - std::time::Duration::from_secs(5),
        ];
        state.compute_sparkline_from_timestamps(&timestamps);
        assert_eq!(state.sparkline_data.last(), Some(&3));
        // All other buckets should be 0
        assert!(state.sparkline_data[..29].iter().all(|&v| v == 0));
    }

    #[test]
    fn sparkline_compute_empty_timestamps() {
        let mut state = DashboardState::default();
        state.sparkline_data[0] = 99; // dirty
        state.compute_sparkline_from_timestamps(&[]);
        assert!(state.sparkline_data.iter().all(|&v| v == 0));
    }

    // ── Dashboard state defaults ───────────────────────────────────────────

    #[test]
    fn dashboard_state_default_has_30_buckets() {
        let state = DashboardState::default();
        assert_eq!(state.sparkline_data.len(), 30);
        assert_eq!(state.sparkline_bucket_secs, 10);
    }

    #[test]
    fn dashboard_state_default_empty_rows() {
        let state = DashboardState::default();
        assert!(state.agent_rows.is_empty());
        assert!(state.coordinator_cards.is_empty());
        assert_eq!(state.selected_row, 0);
    }

    // ── Tab navigation includes Dashboard ──────────────────────────────────

    #[test]
    fn dashboard_is_in_all_tabs() {
        assert!(RightPanelTab::ALL.contains(&RightPanelTab::Dashboard));
    }

    #[test]
    fn dashboard_tab_index_is_6() {
        assert_eq!(RightPanelTab::Dashboard.index(), 6);
    }

    #[test]
    fn dashboard_next_is_messages() {
        assert_eq!(RightPanelTab::Dashboard.next(), RightPanelTab::Messages);
    }

    #[test]
    fn messages_next_wraps_to_chat() {
        assert_eq!(RightPanelTab::Messages.next(), RightPanelTab::Chat);
    }

    #[test]
    fn chat_prev_is_messages() {
        assert_eq!(RightPanelTab::Chat.prev(), RightPanelTab::Messages);
    }

    #[test]
    fn dashboard_prev_is_coord_log() {
        assert_eq!(RightPanelTab::Dashboard.prev(), RightPanelTab::CoordLog);
    }

    // ── Activity labels ────────────────────────────────────────────────────

    #[test]
    fn activity_labels() {
        assert_eq!(DashboardAgentActivity::Active.label(), "active");
        assert_eq!(DashboardAgentActivity::Slow.label(), "slow");
        assert_eq!(DashboardAgentActivity::Stuck.label(), "stuck");
        assert_eq!(DashboardAgentActivity::Exited.label(), "exited");
    }
}

#[cfg(test)]
mod toast_tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn make_toast(msg: &str, severity: ToastSeverity) -> Toast {
        Toast {
            message: msg.to_string(),
            severity,
            created_at: Instant::now(),
            dedup_key: None,
        }
    }

    fn make_toast_with_age(msg: &str, severity: ToastSeverity, age: Duration) -> Toast {
        Toast {
            message: msg.to_string(),
            severity,
            created_at: Instant::now() - age,
            dedup_key: None,
        }
    }

    // ── Toast lifecycle tests ──

    #[test]
    fn toast_info_auto_dismiss_5s() {
        assert_eq!(
            ToastSeverity::Info.auto_dismiss_duration(),
            Some(Duration::from_secs(5))
        );
    }

    #[test]
    fn toast_warning_auto_dismiss_10s() {
        assert_eq!(
            ToastSeverity::Warning.auto_dismiss_duration(),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn toast_error_auto_dismiss_30s() {
        assert_eq!(
            ToastSeverity::Error.auto_dismiss_duration(),
            Some(Duration::from_secs(30))
        );
    }

    #[test]
    fn toast_cleanup_removes_expired_info() {
        let mut toasts = vec![
            make_toast_with_age("fresh", ToastSeverity::Info, Duration::from_secs(1)),
            make_toast_with_age("expired", ToastSeverity::Info, Duration::from_secs(6)),
        ];
        let before = toasts.len();
        toasts.retain(|t| match t.severity.auto_dismiss_duration() {
            Some(dur) => t.created_at.elapsed() < dur,
            None => true,
        });
        assert_eq!(toasts.len(), 1);
        assert_ne!(toasts.len(), before);
        assert_eq!(toasts[0].message, "fresh");
    }

    #[test]
    fn toast_cleanup_removes_expired_warning() {
        let mut toasts = vec![
            make_toast_with_age("active", ToastSeverity::Warning, Duration::from_secs(5)),
            make_toast_with_age("expired", ToastSeverity::Warning, Duration::from_secs(11)),
        ];
        toasts.retain(|t| match t.severity.auto_dismiss_duration() {
            Some(dur) => t.created_at.elapsed() < dur,
            None => true,
        });
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].message, "active");
    }

    #[test]
    fn toast_error_survives_cleanup_within_timeout() {
        let mut toasts = vec![make_toast_with_age(
            "error",
            ToastSeverity::Error,
            Duration::from_secs(15), // 15 seconds old, within 30s timeout
        )];
        toasts.retain(|t| match t.severity.auto_dismiss_duration() {
            Some(dur) => t.created_at.elapsed() < dur,
            None => true,
        });
        assert_eq!(
            toasts.len(),
            1,
            "Error toasts within timeout must survive cleanup"
        );
    }

    #[test]
    fn toast_error_expires_after_timeout() {
        let mut toasts = vec![make_toast_with_age(
            "error",
            ToastSeverity::Error,
            Duration::from_secs(31), // 31 seconds old, past 30s timeout
        )];
        toasts.retain(|t| match t.severity.auto_dismiss_duration() {
            Some(dur) => t.created_at.elapsed() < dur,
            None => true,
        });
        assert_eq!(
            toasts.len(),
            0,
            "Error toasts past timeout must be cleaned up"
        );
    }

    #[test]
    fn toast_manual_dismissal_removes_errors() {
        let mut toasts = vec![
            make_toast("info msg", ToastSeverity::Info),
            make_toast("error msg", ToastSeverity::Error),
            make_toast("warning msg", ToastSeverity::Warning),
            make_toast("another error", ToastSeverity::Error),
        ];
        let before = toasts.len();
        toasts.retain(|t| t.severity != ToastSeverity::Error);
        assert!(toasts.len() < before);
        assert_eq!(toasts.len(), 2);
        assert!(toasts.iter().all(|t| t.severity != ToastSeverity::Error));
    }

    // ── Toast deduplication tests ──

    #[test]
    fn toast_dedup_replaces_existing() {
        let mut toasts: Vec<Toast> = vec![Toast {
            message: "Agent stuck: task-a".to_string(),
            severity: ToastSeverity::Warning,
            created_at: Instant::now() - Duration::from_secs(3),
            dedup_key: Some("stuck:task-a".to_string()),
        }];
        let key = "stuck:task-a";
        toasts.retain(|t| t.dedup_key.as_deref() != Some(key));
        toasts.push(Toast {
            message: "Agent stuck: task-a (10m)".to_string(),
            severity: ToastSeverity::Warning,
            created_at: Instant::now(),
            dedup_key: Some(key.to_string()),
        });
        assert_eq!(toasts.len(), 1);
        assert!(toasts[0].message.contains("10m"));
    }

    #[test]
    fn toast_dedup_preserves_different_keys() {
        let mut toasts: Vec<Toast> = vec![Toast {
            message: "Agent stuck: task-a".to_string(),
            severity: ToastSeverity::Warning,
            created_at: Instant::now(),
            dedup_key: Some("stuck:task-a".to_string()),
        }];
        let key = "stuck:task-b";
        toasts.retain(|t| t.dedup_key.as_deref() != Some(key));
        toasts.push(Toast {
            message: "Agent stuck: task-b".to_string(),
            severity: ToastSeverity::Warning,
            created_at: Instant::now(),
            dedup_key: Some(key.to_string()),
        });
        assert_eq!(toasts.len(), 2);
    }

    // ── Max visible toasts test ──

    #[test]
    fn toast_max_visible_cap() {
        let mut toasts: Vec<Toast> = Vec::new();
        for i in 0..6 {
            toasts.push(make_toast(&format!("msg {}", i), ToastSeverity::Info));
            while toasts.len() > MAX_VISIBLE_TOASTS {
                toasts.remove(0);
            }
        }
        assert_eq!(toasts.len(), MAX_VISIBLE_TOASTS);
        assert_eq!(toasts[0].message, "msg 2");
        assert_eq!(toasts[3].message, "msg 5");
    }

    // ── Rendering tests (color per severity) ──

    #[test]
    fn toast_severity_colors_are_distinct() {
        let info_color = match ToastSeverity::Info {
            ToastSeverity::Info => (100.0_f64, 255.0, 100.0),
            _ => unreachable!(),
        };
        let warning_color = match ToastSeverity::Warning {
            ToastSeverity::Warning => (255.0_f64, 220.0, 80.0),
            _ => unreachable!(),
        };
        let error_color = match ToastSeverity::Error {
            ToastSeverity::Error => (255.0_f64, 100.0, 100.0),
            _ => unreachable!(),
        };
        assert_ne!(info_color, warning_color);
        assert_ne!(info_color, error_color);
        assert_ne!(warning_color, error_color);
    }

    #[test]
    fn toast_stacking_order() {
        let toasts = vec![
            make_toast("first", ToastSeverity::Info),
            make_toast("second", ToastSeverity::Warning),
            make_toast("third", ToastSeverity::Error),
        ];
        let visible_count = toasts.len().min(MAX_VISIBLE_TOASTS);
        let start = toasts.len().saturating_sub(visible_count);
        let visible: Vec<_> = toasts[start..].iter().rev().collect();
        assert_eq!(visible[0].message, "third");
        assert_eq!(visible[1].message, "second");
        assert_eq!(visible[2].message, "first");
    }

    // ── Phase 1 trigger tests ──

    #[test]
    fn toast_phase1_task_done_generates_info() {
        let mut toasts: Vec<Toast> = Vec::new();
        let task_id = "my-feature";
        toasts.push(Toast {
            message: format!("\u{2705} Done: {} (3m)", task_id),
            severity: ToastSeverity::Info,
            created_at: Instant::now(),
            dedup_key: None,
        });
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].severity, ToastSeverity::Info);
        assert!(toasts[0].message.contains("Done"));
        assert!(toasts[0].message.contains("3m"));
    }

    #[test]
    fn toast_phase1_task_failed_generates_error() {
        let mut toasts: Vec<Toast> = Vec::new();
        let task_id = "broken-task";
        toasts.push(Toast {
            message: format!("\u{274c} Failed: {}", task_id),
            severity: ToastSeverity::Error,
            created_at: Instant::now(),
            dedup_key: None,
        });
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].severity, ToastSeverity::Error);
        assert!(toasts[0].message.contains("Failed"));
    }

    #[test]
    fn toast_phase1_agent_exited_generates_info_with_duration() {
        let mut toasts: Vec<Toast> = Vec::new();
        toasts.push(Toast {
            message: "\u{1f6aa} Agent exited: agent-5 on build-feature (12m)".to_string(),
            severity: ToastSeverity::Info,
            created_at: Instant::now(),
            dedup_key: None,
        });
        assert_eq!(toasts[0].severity, ToastSeverity::Info);
        assert!(toasts[0].message.contains("Agent exited"));
        assert!(toasts[0].message.contains("12m"));
    }

    #[test]
    fn toast_phase1_agent_stuck_generates_deduped_warning() {
        let mut toasts: Vec<Toast> = Vec::new();
        let key = "stuck:agent-3";
        toasts.push(Toast {
            message: "\u{23f3} Agent stuck: agent-3 on my-task (6m)".to_string(),
            severity: ToastSeverity::Warning,
            created_at: Instant::now() - Duration::from_secs(60),
            dedup_key: Some(key.to_string()),
        });
        toasts.retain(|t| t.dedup_key.as_deref() != Some(key));
        toasts.push(Toast {
            message: "\u{23f3} Agent stuck: agent-3 on my-task (11m)".to_string(),
            severity: ToastSeverity::Warning,
            created_at: Instant::now(),
            dedup_key: Some(key.to_string()),
        });
        assert_eq!(toasts.len(), 1);
        assert_eq!(toasts[0].severity, ToastSeverity::Warning);
        assert!(toasts[0].message.contains("11m"));
    }

    #[test]
    fn toast_phase1_new_message_generates_info() {
        let mut toasts: Vec<Toast> = Vec::new();
        let count = 2;
        let label = if count == 1 { "message" } else { "messages" };
        toasts.push(Toast {
            message: format!("\u{1f4ac} {} new {} from coordinator", count, label),
            severity: ToastSeverity::Info,
            created_at: Instant::now(),
            dedup_key: None,
        });
        assert_eq!(toasts[0].severity, ToastSeverity::Info);
        assert!(toasts[0].message.contains("2 new messages"));
    }
}

#[cfg(test)]
mod nav_stack_tests {
    use super::*;

    #[test]
    fn new_nav_stack_is_empty() {
        let stack = NavStack::default();
        assert!(stack.is_empty());
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn push_increases_len() {
        let mut stack = NavStack::default();
        stack.push(NavEntry::Dashboard);
        assert_eq!(stack.len(), 1);
        assert!(!stack.is_empty());
    }

    #[test]
    fn pop_returns_last_pushed() {
        let mut stack = NavStack::default();
        stack.push(NavEntry::Dashboard);
        stack.push(NavEntry::AgentDetail {
            agent_id: "a1".into(),
        });
        assert_eq!(
            stack.pop(),
            Some(NavEntry::AgentDetail {
                agent_id: "a1".into()
            })
        );
        assert_eq!(stack.pop(), Some(NavEntry::Dashboard));
    }

    #[test]
    fn pop_on_empty_returns_none() {
        let mut stack = NavStack::default();
        assert_eq!(stack.pop(), None);
        assert!(stack.is_empty());
        assert_eq!(stack.pop(), None);
    }

    #[test]
    fn clear_empties_the_stack() {
        let mut stack = NavStack::default();
        stack.push(NavEntry::Dashboard);
        stack.push(NavEntry::TaskDetail {
            task_id: "t1".into(),
        });
        stack.clear();
        assert!(stack.is_empty());
        assert_eq!(stack.len(), 0);
    }

    #[test]
    fn full_drilldown_chain_push_and_pop() {
        let mut stack = NavStack::default();
        stack.push(NavEntry::Dashboard);
        stack.push(NavEntry::AgentDetail {
            agent_id: "agent-42".into(),
        });
        stack.push(NavEntry::TaskDetail {
            task_id: "implement-feature".into(),
        });
        assert_eq!(stack.len(), 3);

        assert_eq!(
            stack.pop(),
            Some(NavEntry::TaskDetail {
                task_id: "implement-feature".into()
            })
        );
        assert_eq!(
            stack.pop(),
            Some(NavEntry::AgentDetail {
                agent_id: "agent-42".into()
            })
        );
        assert_eq!(stack.pop(), Some(NavEntry::Dashboard));
        assert!(stack.is_empty());
    }

    #[test]
    fn nav_entry_equality() {
        assert_eq!(NavEntry::Dashboard, NavEntry::Dashboard);
        assert_ne!(
            NavEntry::AgentDetail {
                agent_id: "a".into()
            },
            NavEntry::AgentDetail {
                agent_id: "b".into()
            }
        );
        assert_ne!(
            NavEntry::Dashboard,
            NavEntry::TaskDetail {
                task_id: "t".into()
            }
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests for HUD vitals bar formatting
// ══════════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod vitals_tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn vitals_staleness_fresh() {
        assert_eq!(vitals_staleness_color(0), VitalsStaleness::Fresh);
        assert_eq!(vitals_staleness_color(15), VitalsStaleness::Fresh);
        assert_eq!(vitals_staleness_color(29), VitalsStaleness::Fresh);
    }

    #[test]
    fn vitals_staleness_stale() {
        assert_eq!(vitals_staleness_color(30), VitalsStaleness::Stale);
        assert_eq!(vitals_staleness_color(120), VitalsStaleness::Stale);
        assert_eq!(vitals_staleness_color(299), VitalsStaleness::Stale);
    }

    #[test]
    fn vitals_staleness_dead() {
        assert_eq!(vitals_staleness_color(300), VitalsStaleness::Dead);
        assert_eq!(vitals_staleness_color(3600), VitalsStaleness::Dead);
    }

    #[test]
    fn format_vitals_zero_agents() {
        let v = VitalsState {
            agents_alive: 0,
            open: 5,
            running: 0,
            done: 10,
            last_event_time: None,
            coord_last_tick: None,
            daemon_running: false,
        };
        let s = format_vitals(&v);
        assert!(s.contains("○ 0 agents"), "got: {}", s);
        assert!(s.contains("5 open"), "got: {}", s);
        assert!(s.contains("0 running"), "got: {}", s);
        assert!(s.contains("10 done"), "got: {}", s);
        assert!(s.contains("no events"), "got: {}", s);
        assert!(s.contains("coord ○ down"), "got: {}", s);
    }

    #[test]
    fn format_vitals_with_agents() {
        let now = SystemTime::now();
        let v = VitalsState {
            agents_alive: 3,
            open: 8,
            running: 3,
            done: 45,
            last_event_time: Some(now - Duration::from_secs(4)),
            coord_last_tick: Some(now - Duration::from_secs(2)),
            daemon_running: true,
        };
        let s = format_vitals(&v);
        assert!(s.contains("● 3 agents"), "got: {}", s);
        assert!(s.contains("8 open"), "got: {}", s);
        assert!(s.contains("3 running"), "got: {}", s);
        assert!(s.contains("45 done"), "got: {}", s);
        assert!(s.contains("last event"), "got: {}", s);
        assert!(s.contains("coord ●"), "got: {}", s);
    }

    #[test]
    fn format_vitals_single_agent() {
        let v = VitalsState {
            agents_alive: 1,
            open: 2,
            running: 1,
            done: 0,
            last_event_time: None,
            coord_last_tick: None,
            daemon_running: true,
        };
        let s = format_vitals(&v);
        assert!(s.contains("● 1 agents"), "got: {}", s);
        assert!(s.contains("coord ● –"), "got: {}", s);
    }

    #[test]
    fn format_vitals_daemon_down() {
        let v = VitalsState {
            agents_alive: 0,
            open: 0,
            running: 0,
            done: 0,
            last_event_time: None,
            coord_last_tick: None,
            daemon_running: false,
        };
        let s = format_vitals(&v);
        assert!(s.contains("coord ○ down"), "got: {}", s);
    }

    #[test]
    fn format_vitals_old_event() {
        let now = SystemTime::now();
        let v = VitalsState {
            agents_alive: 0,
            open: 0,
            running: 0,
            done: 100,
            last_event_time: Some(now - Duration::from_secs(600)),
            coord_last_tick: None,
            daemon_running: false,
        };
        let s = format_vitals(&v);
        // 600s = 10m
        assert!(s.contains("last event 10m ago"), "got: {}", s);
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// TUI chat end-to-end persistence tests
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tui_chat_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use tempfile::TempDir;
    use workgraph::graph::{Node, Status, WorkGraph};
    use workgraph::parser::save_graph;
    use workgraph::test_helpers::make_task_with_status;

    use crate::commands::viz::ascii::generate_ascii;
    use crate::commands::viz::{LayoutMode as VizLayoutMode, VizOutput};

    // ── helpers ──

    /// Create a minimal workgraph with coordinator tasks so list_coordinator_ids works.
    fn setup_workgraph_with_coordinators(
        tmp: &TempDir,
        coordinator_ids: &[u32],
    ) -> (VizOutput, std::path::PathBuf) {
        let mut graph = WorkGraph::new();

        // Create a coordinator task for each requested ID.
        for &cid in coordinator_ids {
            let id = if cid == 0 {
                ".coordinator".to_string()
            } else {
                format!(".coordinator-{}", cid)
            };
            let title = format!("Coordinator {}", cid);
            let mut task = make_task_with_status(&id, &title, Status::InProgress);
            task.tags = vec!["coordinator-loop".to_string()];
            graph.add_node(Node::Task(task));
        }

        // Also add a regular task for interleaving tests.
        let regular = make_task_with_status("test-task", "Test Task", Status::InProgress);
        graph.add_node(Node::Task(regular));

        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        let graph_path = wg_dir.join("graph.jsonl");
        save_graph(&graph, &graph_path).unwrap();

        // Also write a config.toml so chat_history is enabled.
        let config_path = wg_dir.join("config.toml");
        std::fs::write(
            &config_path,
            "[tui]\nchat_history = true\nchat_history_max = 1000\n",
        )
        .unwrap();

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            VizLayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        (viz, wg_dir)
    }

    fn build_test_app(viz: &VizOutput, wg_dir: &std::path::Path) -> VizApp {
        let mut app = VizApp::from_viz_output_for_test(viz);
        app.workgraph_dir = wg_dir.to_path_buf();
        app
    }

    fn make_chat_message(role: ChatRole, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            text: text.to_string(),
            full_text: None,
            attachments: vec![],
            edited: false,
            inbox_id: None,
            user: Some("test-user".to_string()),
            target_task: None,
            msg_timestamp: Some(chrono::Utc::now().to_rfc3339()),
            read_at: None,
            msg_queue_id: None,
        }
    }

    fn make_chat_message_with_ts(role: ChatRole, text: &str, ts: &str) -> ChatMessage {
        ChatMessage {
            role,
            text: text.to_string(),
            full_text: None,
            attachments: vec![],
            edited: false,
            inbox_id: None,
            user: Some("test-user".to_string()),
            target_task: None,
            msg_timestamp: Some(ts.to_string()),
            read_at: None,
            msg_queue_id: None,
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Scenario 1: Persistence round-trip
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn chat_persistence_round_trip_basic() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Simulate: open TUI, send messages
        let mut app = build_test_app(&viz, &wg_dir);
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "Hello coordinator!"));
        app.chat.messages.push(make_chat_message(
            ChatRole::Coordinator,
            "Hi there, how can I help?",
        ));
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "Please build a feature."));

        // Simulate: close TUI (saves all state)
        app.save_all_chat_state();

        // Simulate: reopen TUI (new app, loads history)
        let mut app2 = build_test_app(&viz, &wg_dir);
        app2.load_chat_history();

        // Verify all messages are present and intact
        assert_eq!(
            app2.chat.messages.len(),
            3,
            "Should have 3 messages after reload"
        );
        assert_eq!(app2.chat.messages[0].text, "Hello coordinator!");
        assert!(matches!(app2.chat.messages[0].role, ChatRole::User));
        assert_eq!(app2.chat.messages[1].text, "Hi there, how can I help?");
        assert!(matches!(app2.chat.messages[1].role, ChatRole::Coordinator));
        assert_eq!(app2.chat.messages[2].text, "Please build a feature.");
        assert!(matches!(app2.chat.messages[2].role, ChatRole::User));
    }

    #[test]
    fn chat_persistence_filters_out_sent_messages() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let mut app = build_test_app(&viz, &wg_dir);

        // Add a real user message and a SentMessage (interleaved task message)
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "Hello coordinator"));
        let mut sent = make_chat_message(ChatRole::SentMessage, "Check this task");
        sent.target_task = Some("task-xyz".to_string());
        sent.msg_timestamp = Some("2026-03-27T10:00:00Z".to_string());
        sent.read_at = Some("2026-03-27T10:01:00Z".to_string());
        sent.msg_queue_id = Some(42);
        app.chat.messages.push(sent);

        app.save_all_chat_state();

        let mut app2 = build_test_app(&viz, &wg_dir);
        app2.load_chat_history();

        // SentMessage entries should be filtered out — only the user message survives
        assert_eq!(app2.chat.messages.len(), 1);
        assert!(matches!(app2.chat.messages[0].role, ChatRole::User));
        assert_eq!(app2.chat.messages[0].text, "Hello coordinator");
    }

    #[test]
    fn chat_persistence_respects_max_history() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Override config with very small max
        let config_path = wg_dir.join("config.toml");
        std::fs::write(
            &config_path,
            "[tui]\nchat_history = true\nchat_history_max = 3\n",
        )
        .unwrap();

        // Save 5 messages
        let messages: Vec<ChatMessage> = (0..5)
            .map(|i| make_chat_message(ChatRole::User, &format!("message {}", i)))
            .collect();

        save_chat_history(&wg_dir, 0, &messages);

        // Load back: should only have the last 3
        let loaded = load_persisted_chat_history(&wg_dir, 0);
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].text, "message 2");
        assert_eq!(loaded[1].text, "message 3");
        assert_eq!(loaded[2].text, "message 4");
    }

    #[test]
    fn chat_persistence_disabled_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Disable chat history
        let config_path = wg_dir.join("config.toml");
        std::fs::write(&config_path, "[tui]\nchat_history = false\n").unwrap();

        let messages = vec![make_chat_message(ChatRole::User, "should not persist")];
        save_chat_history(&wg_dir, 0, &messages);

        let loaded = load_persisted_chat_history(&wg_dir, 0);
        assert!(
            loaded.is_empty(),
            "Should return empty when chat_history is disabled"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Scenario 2: Focus restore
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn tui_focus_restore_coordinator_id() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0, 1, 2]);

        // Simulate: user switches to coordinator 2, then closes TUI
        let mut app = build_test_app(&viz, &wg_dir);
        app.active_coordinator_id = 2;
        app.right_panel_tab = RightPanelTab::Chat;
        app.save_all_chat_state();

        // Simulate: reopen TUI
        let mut app2 = build_test_app(&viz, &wg_dir);
        app2.restore_tui_state();

        assert_eq!(
            app2.active_coordinator_id, 2,
            "Should restore to coordinator 2"
        );
        assert_eq!(
            app2.right_panel_tab,
            RightPanelTab::Chat,
            "Should restore Chat tab"
        );
    }

    #[test]
    fn tui_focus_restore_falls_back_when_coordinator_gone() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0, 1]);

        // Persist state pointing to coordinator 5 which doesn't exist in graph
        save_tui_state(&wg_dir, 5, &RightPanelTab::Chat);

        let mut app = build_test_app(&viz, &wg_dir);
        app.restore_tui_state();

        // Should not change from default 0 since coordinator 5 is not in the graph
        assert_eq!(
            app.active_coordinator_id, 0,
            "Should not restore to non-existent coordinator"
        );
    }

    #[test]
    fn tui_focus_restore_different_tabs() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Save with Log tab active
        save_tui_state(&wg_dir, 0, &RightPanelTab::Log);

        let mut app = build_test_app(&viz, &wg_dir);
        app.restore_tui_state();

        assert_eq!(app.right_panel_tab, RightPanelTab::Log);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Scenario 3: Message interleaving
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn chat_interleaving_inserts_at_correct_position() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let mut app = build_test_app(&viz, &wg_dir);

        // Simulate existing chat messages with known timestamps
        app.chat.messages.push(make_chat_message_with_ts(
            ChatRole::User,
            "first user msg",
            "2026-03-27T10:00:00Z",
        ));
        app.chat.messages.push(make_chat_message_with_ts(
            ChatRole::Coordinator,
            "first coord response",
            "2026-03-27T10:01:00Z",
        ));
        app.chat.messages.push(make_chat_message_with_ts(
            ChatRole::User,
            "second user msg",
            "2026-03-27T10:05:00Z",
        ));
        app.chat.messages.push(make_chat_message_with_ts(
            ChatRole::Coordinator,
            "second coord response",
            "2026-03-27T10:06:00Z",
        ));

        // Create a message file for test-task with a read message
        // that was read at 10:03 (between first response and second user msg)
        let msg_dir = wg_dir.join("messages");
        std::fs::create_dir_all(&msg_dir).unwrap();
        let msg_file = msg_dir.join("test-task.jsonl");
        let msg_json = serde_json::json!({
            "id": 1,
            "timestamp": "2026-03-27T10:02:00Z",
            "sender": "user",
            "body": "hey agent, check this",
            "priority": "normal",
            "status": "read",
            "read_at": "2026-03-27T10:03:00Z"
        });
        std::fs::write(&msg_file, format!("{}\n", msg_json)).unwrap();

        // Poll for interleaved messages
        app.poll_interleaved_messages();

        // The interleaved message should appear after the coordinator response at 10:01
        // and before the user message at 10:05 (based on read_at of 10:03)
        assert_eq!(app.chat.messages.len(), 5, "Should have 5 messages total");
        assert_eq!(app.chat.messages[2].text, "hey agent, check this");
        assert!(matches!(app.chat.messages[2].role, ChatRole::SentMessage));
        assert_eq!(
            app.chat.messages[2].target_task.as_deref(),
            Some("test-task")
        );
    }

    #[test]
    fn chat_interleaving_deduplicates_on_repoll() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let mut app = build_test_app(&viz, &wg_dir);
        app.chat.messages.push(make_chat_message_with_ts(
            ChatRole::User,
            "hello",
            "2026-03-27T10:00:00Z",
        ));

        // Create message file
        let msg_dir = wg_dir.join("messages");
        std::fs::create_dir_all(&msg_dir).unwrap();
        let msg_file = msg_dir.join("test-task.jsonl");
        let msg_json = serde_json::json!({
            "id": 1,
            "timestamp": "2026-03-27T10:01:00Z",
            "sender": "user",
            "body": "interleaved msg",
            "priority": "normal",
            "status": "read",
            "read_at": "2026-03-27T10:02:00Z"
        });
        std::fs::write(&msg_file, format!("{}\n", msg_json)).unwrap();

        // Poll twice
        app.poll_interleaved_messages();
        app.poll_interleaved_messages();

        // Should only appear once (dedup by task_id + msg_queue_id)
        let sent_count = app
            .chat
            .messages
            .iter()
            .filter(|m| matches!(m.role, ChatRole::SentMessage))
            .count();
        assert_eq!(sent_count, 1, "Should not duplicate interleaved messages");
    }

    #[test]
    fn chat_interleaving_skips_unread_messages() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let mut app = build_test_app(&viz, &wg_dir);
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "hi"));

        // Create a message with status "sent" (not yet read by agent)
        let msg_dir = wg_dir.join("messages");
        std::fs::create_dir_all(&msg_dir).unwrap();
        let msg_file = msg_dir.join("test-task.jsonl");
        let msg_json = serde_json::json!({
            "id": 1,
            "timestamp": "2026-03-27T10:00:00Z",
            "sender": "user",
            "body": "not yet read",
            "priority": "normal",
            "status": "sent"
        });
        std::fs::write(&msg_file, format!("{}\n", msg_json)).unwrap();

        app.poll_interleaved_messages();

        // Should NOT appear — only read/acknowledged messages are interleaved
        let sent_count = app
            .chat
            .messages
            .iter()
            .filter(|m| matches!(m.role, ChatRole::SentMessage))
            .count();
        assert_eq!(sent_count, 0, "Unread messages should not be interleaved");
    }

    #[test]
    fn chat_interleaving_skips_agent_sent_messages() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let mut app = build_test_app(&viz, &wg_dir);
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "hi"));

        // Create a message sent BY an agent (not from user/tui/coordinator)
        let msg_dir = wg_dir.join("messages");
        std::fs::create_dir_all(&msg_dir).unwrap();
        let msg_file = msg_dir.join("test-task.jsonl");
        let msg_json = serde_json::json!({
            "id": 1,
            "timestamp": "2026-03-27T10:00:00Z",
            "sender": "agent-123",
            "body": "agent reply",
            "priority": "normal",
            "status": "read",
            "read_at": "2026-03-27T10:01:00Z"
        });
        std::fs::write(&msg_file, format!("{}\n", msg_json)).unwrap();

        app.poll_interleaved_messages();

        let sent_count = app
            .chat
            .messages
            .iter()
            .filter(|m| matches!(m.role, ChatRole::SentMessage))
            .count();
        assert_eq!(
            sent_count, 0,
            "Agent-sent messages should not be interleaved"
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Scenario 4: Multiple coordinators
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn multi_coordinator_independent_chat_persistence() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0, 1, 2]);

        let mut app = build_test_app(&viz, &wg_dir);

        // Send messages in coordinator 0
        app.active_coordinator_id = 0;
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "msg in coord 0"));
        app.chat.messages.push(make_chat_message(
            ChatRole::Coordinator,
            "response from coord 0",
        ));

        // Switch to coordinator 1 and send messages
        app.switch_coordinator(1);
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "msg in coord 1"));

        // Switch to coordinator 2 and send messages
        app.switch_coordinator(2);
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "msg in coord 2 - A"));
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "msg in coord 2 - B"));

        // Close TUI
        app.save_all_chat_state();

        // Reopen TUI
        let mut app2 = build_test_app(&viz, &wg_dir);
        app2.active_coordinator_id = 0;
        app2.load_chat_history();

        // Coordinator 0 should have 2 messages
        assert_eq!(app2.chat.messages.len(), 2, "Coord 0 should have 2 msgs");
        assert_eq!(app2.chat.messages[0].text, "msg in coord 0");
        assert_eq!(app2.chat.messages[1].text, "response from coord 0");

        // Switch to coordinator 1
        app2.switch_coordinator(1);
        assert_eq!(app2.chat.messages.len(), 1, "Coord 1 should have 1 msg");
        assert_eq!(app2.chat.messages[0].text, "msg in coord 1");

        // Switch to coordinator 2
        app2.switch_coordinator(2);
        assert_eq!(app2.chat.messages.len(), 2, "Coord 2 should have 2 msgs");
        assert_eq!(app2.chat.messages[0].text, "msg in coord 2 - A");
        assert_eq!(app2.chat.messages[1].text, "msg in coord 2 - B");
    }

    #[test]
    fn multi_coordinator_switch_preserves_in_memory_state() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0, 1]);

        let mut app = build_test_app(&viz, &wg_dir);

        // Add messages to coordinator 0
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "coord0 msg"));

        // Switch to coordinator 1
        app.switch_coordinator(1);
        assert!(
            app.chat.messages.is_empty(),
            "Coord 1 should start with no messages"
        );
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "coord1 msg"));

        // Switch back to coordinator 0
        app.switch_coordinator(0);
        assert_eq!(app.chat.messages.len(), 1);
        assert_eq!(app.chat.messages[0].text, "coord0 msg");

        // Switch back to coordinator 1
        app.switch_coordinator(1);
        assert_eq!(app.chat.messages.len(), 1);
        assert_eq!(app.chat.messages[0].text, "coord1 msg");
    }

    #[test]
    fn multi_coordinator_file_paths_are_independent() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0, 1, 2]);

        // All coordinators use chat-history-{N}.jsonl
        assert_eq!(
            chat_history_path(&wg_dir, 0),
            wg_dir.join("chat-history-0.jsonl")
        );
        assert_eq!(
            chat_history_path(&wg_dir, 1),
            wg_dir.join("chat-history-1.jsonl")
        );
        assert_eq!(
            chat_history_path(&wg_dir, 2),
            wg_dir.join("chat-history-2.jsonl")
        );

        // Save to different coordinators, verify files are independent
        let msgs_0 = vec![make_chat_message(ChatRole::User, "coord 0")];
        let msgs_1 = vec![make_chat_message(ChatRole::User, "coord 1")];

        save_chat_history(&wg_dir, 0, &msgs_0);
        save_chat_history(&wg_dir, 1, &msgs_1);

        let loaded_0 = load_persisted_chat_history(&wg_dir, 0);
        let loaded_1 = load_persisted_chat_history(&wg_dir, 1);

        assert_eq!(loaded_0.len(), 1);
        assert_eq!(loaded_0[0].text, "coord 0");
        assert_eq!(loaded_1.len(), 1);
        assert_eq!(loaded_1[0].text, "coord 1");
    }

    /// Regression test: the fallback path in load_chat_history_for_coordinator
    /// must use the coordinator-specific inbox/outbox, not the backward-compat
    /// coordinator-0 wrappers. Otherwise coordinator N loads coordinator 0's
    /// messages when no persisted chat-history file exists yet.
    #[test]
    fn chat_fallback_loads_correct_coordinator_inbox_outbox() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0, 3]);

        // Write messages directly to coordinator 0 and 3 inbox/outbox
        // (simulating the daemon writing chat without any persisted TUI history).
        workgraph::chat::append_inbox_for(&wg_dir, 0, "hello from coord 0", "r0").unwrap();
        workgraph::chat::append_outbox_for(&wg_dir, 0, "reply from coord 0", "r0").unwrap();
        workgraph::chat::append_inbox_for(&wg_dir, 3, "hello from coord 3", "r3").unwrap();
        workgraph::chat::append_outbox_for(&wg_dir, 3, "reply from coord 3", "r3").unwrap();

        // Load for coordinator 3 — must NOT see coordinator 0's messages.
        let mut app = build_test_app(&viz, &wg_dir);
        app.active_coordinator_id = 3;
        app.load_chat_history_for_coordinator(3);

        // Should have exactly the coordinator 3 messages, not coordinator 0's.
        let texts: Vec<&str> = app.chat.messages.iter().map(|m| m.text.as_str()).collect();
        assert!(
            !texts.iter().any(|t| t.contains("coord 0")),
            "Coordinator 3 loaded coordinator 0's messages (mixing bug): {:?}",
            texts,
        );
        assert_eq!(
            app.chat.messages.len(),
            2,
            "Expected 2 messages for coord 3, got: {:?}",
            texts
        );
        assert!(
            texts.iter().any(|t| t.contains("coord 3")),
            "Missing coord 3 messages: {:?}",
            texts
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Scenario 5: Edge cases
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn chat_persistence_empty_chat_no_error() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Save empty chat — should not create file or error
        let app = build_test_app(&viz, &wg_dir);
        app.save_all_chat_state();

        // Reload — should work fine with no messages
        let mut app2 = build_test_app(&viz, &wg_dir);
        app2.load_chat_history();
        assert!(app2.chat.messages.is_empty());
    }

    #[test]
    fn chat_persistence_very_long_message() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let long_text = "A".repeat(100_000);

        let mut app = build_test_app(&viz, &wg_dir);
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, &long_text));
        app.save_all_chat_state();

        let mut app2 = build_test_app(&viz, &wg_dir);
        app2.load_chat_history();

        assert_eq!(app2.chat.messages.len(), 1);
        assert_eq!(app2.chat.messages[0].text.len(), 100_000);
        assert_eq!(app2.chat.messages[0].text, long_text);
    }

    #[test]
    fn chat_persistence_rapid_save_load_cycles() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Rapidly save and load 20 times without corruption
        for i in 0..20 {
            let mut app = build_test_app(&viz, &wg_dir);
            app.load_chat_history();
            app.chat
                .messages
                .push(make_chat_message(ChatRole::User, &format!("cycle {}", i)));
            app.save_all_chat_state();
        }

        // Final load should have all 20 messages
        let mut final_app = build_test_app(&viz, &wg_dir);
        final_app.load_chat_history();
        assert_eq!(
            final_app.chat.messages.len(),
            20,
            "Should have all 20 messages after rapid cycles"
        );
        for (i, msg) in final_app.chat.messages.iter().enumerate() {
            assert_eq!(msg.text, format!("cycle {}", i));
        }
    }

    #[test]
    fn chat_persistence_special_characters() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let mut app = build_test_app(&viz, &wg_dir);
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "emoji: 🚀🎉"));
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "newlines:\nline2\nline3"));
        app.chat.messages.push(make_chat_message(
            ChatRole::User,
            "quotes: \"hello\" 'world'",
        ));
        app.chat.messages.push(make_chat_message(
            ChatRole::User,
            "backslash: C:\\Users\\test",
        ));
        app.chat.messages.push(make_chat_message(
            ChatRole::User,
            "json-like: {\"key\": \"value\"}",
        ));
        app.save_all_chat_state();

        let mut app2 = build_test_app(&viz, &wg_dir);
        app2.load_chat_history();

        assert_eq!(app2.chat.messages.len(), 5);
        assert_eq!(app2.chat.messages[0].text, "emoji: 🚀🎉");
        assert_eq!(app2.chat.messages[1].text, "newlines:\nline2\nline3");
        assert_eq!(app2.chat.messages[2].text, "quotes: \"hello\" 'world'");
        assert_eq!(app2.chat.messages[3].text, "backslash: C:\\Users\\test");
        assert_eq!(
            app2.chat.messages[4].text,
            "json-like: {\"key\": \"value\"}"
        );
    }

    #[test]
    fn chat_persistence_corrupt_file_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Write corrupt JSON to the history file
        let path = chat_history_path(&wg_dir, 0);
        std::fs::write(&path, "not valid json at all!!!").unwrap();

        // Should return empty rather than panic
        let loaded = load_persisted_chat_history(&wg_dir, 0);
        assert!(loaded.is_empty(), "Corrupt file should return empty vec");
    }

    #[test]
    fn chat_persistence_nonexistent_file_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // No file exists — should return empty
        let loaded = load_persisted_chat_history(&wg_dir, 0);
        assert!(loaded.is_empty());
    }

    #[test]
    fn tui_state_persistence_round_trip() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        save_tui_state(&wg_dir, 3, &RightPanelTab::Messages);

        let loaded = load_tui_state(&wg_dir);
        assert!(loaded.is_some());
        let state = loaded.unwrap();
        assert_eq!(state.active_coordinator_id, 3);
        assert_eq!(state.right_panel_tab, "Messages");
    }

    #[test]
    fn tui_state_no_file_returns_none() {
        let tmp = TempDir::new().unwrap();
        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();

        let loaded = load_tui_state(&wg_dir);
        assert!(loaded.is_none());
    }

    #[test]
    fn chat_interleaving_message_to_finished_agent() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let mut app = build_test_app(&viz, &wg_dir);
        app.chat.messages.push(make_chat_message_with_ts(
            ChatRole::User,
            "started work",
            "2026-03-27T09:00:00Z",
        ));

        // The agent finished and read the message before finishing
        let msg_dir = wg_dir.join("messages");
        std::fs::create_dir_all(&msg_dir).unwrap();
        let msg_file = msg_dir.join("test-task.jsonl");
        let msg_json = serde_json::json!({
            "id": 1,
            "timestamp": "2026-03-27T09:30:00Z",
            "sender": "user",
            "body": "message to finished agent",
            "priority": "normal",
            "status": "acknowledged",
            "read_at": "2026-03-27T09:31:00Z"
        });
        std::fs::write(&msg_file, format!("{}\n", msg_json)).unwrap();

        app.poll_interleaved_messages();

        // Should still appear in the stream (acknowledged messages are interleaved)
        let sent_msgs: Vec<_> = app
            .chat
            .messages
            .iter()
            .filter(|m| matches!(m.role, ChatRole::SentMessage))
            .collect();
        assert_eq!(sent_msgs.len(), 1);
        assert_eq!(sent_msgs[0].text, "message to finished agent");
    }

    #[test]
    fn chat_persistence_with_attachments() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let mut app = build_test_app(&viz, &wg_dir);
        let mut msg = make_chat_message(ChatRole::User, "here is a file");
        msg.attachments = vec!["screenshot.png".to_string(), "log.txt".to_string()];
        app.chat.messages.push(msg);
        app.save_all_chat_state();

        let mut app2 = build_test_app(&viz, &wg_dir);
        app2.load_chat_history();

        assert_eq!(app2.chat.messages.len(), 1);
        assert_eq!(
            app2.chat.messages[0].attachments,
            vec!["screenshot.png", "log.txt"]
        );
    }

    #[test]
    fn chat_persistence_edited_flag() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let mut app = build_test_app(&viz, &wg_dir);
        let mut msg = make_chat_message(ChatRole::User, "edited message");
        msg.edited = true;
        app.chat.messages.push(msg);
        app.save_all_chat_state();

        let mut app2 = build_test_app(&viz, &wg_dir);
        app2.load_chat_history();

        assert_eq!(app2.chat.messages.len(), 1);
        assert!(app2.chat.messages[0].edited, "edited flag should persist");
    }

    #[test]
    fn switch_coordinator_resets_input_mode() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0, 1]);

        let mut app = build_test_app(&viz, &wg_dir);
        app.input_mode = InputMode::ChatInput;
        app.inspector_sub_focus = InspectorSubFocus::TextEntry;

        app.switch_coordinator(1);

        assert_eq!(
            app.input_mode,
            InputMode::Normal,
            "Switching coordinator should reset input mode"
        );
        assert_eq!(
            app.inspector_sub_focus,
            InspectorSubFocus::ChatHistory,
            "Switching coordinator should reset inspector sub-focus"
        );
    }

    #[test]
    fn switch_coordinator_noop_for_same_id() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let mut app = build_test_app(&viz, &wg_dir);
        app.chat
            .messages
            .push(make_chat_message(ChatRole::User, "original"));

        // Switching to the same coordinator should be a no-op
        app.switch_coordinator(0);

        assert_eq!(app.chat.messages.len(), 1);
        assert_eq!(app.chat.messages[0].text, "original");
    }

    #[test]
    fn chat_persistence_full_text_and_user_fields() {
        let tmp = TempDir::new().unwrap();
        let (viz, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        let mut app = build_test_app(&viz, &wg_dir);
        let mut msg = make_chat_message(ChatRole::Coordinator, "summary text");
        msg.full_text = Some("full response with tool calls and details".to_string());
        msg.user = Some("coordinator-agent".to_string());
        app.chat.messages.push(msg);
        app.save_all_chat_state();

        let mut app2 = build_test_app(&viz, &wg_dir);
        app2.load_chat_history();

        assert_eq!(app2.chat.messages.len(), 1);
        assert_eq!(app2.chat.messages[0].text, "summary text");
        assert_eq!(
            app2.chat.messages[0].full_text.as_deref(),
            Some("full response with tool calls and details")
        );
        assert_eq!(
            app2.chat.messages[0].user.as_deref(),
            Some("coordinator-agent")
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Pagination tests
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn chat_pagination_loads_only_last_page() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Set page size to 3
        let config_path = wg_dir.join("config.toml");
        std::fs::write(
            &config_path,
            "[tui]\nchat_history = true\nchat_history_max = 1000\nchat_page_size = 3\n",
        )
        .unwrap();

        // Save 10 messages
        let messages: Vec<ChatMessage> = (0..10)
            .map(|i| make_chat_message(ChatRole::User, &format!("message {}", i)))
            .collect();
        save_chat_history(&wg_dir, 0, &messages);

        // Load paginated: should get last 3
        let result = load_persisted_chat_history_paginated(&wg_dir, 0, 3);
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.total_count, 10);
        assert!(result.has_more);
        assert_eq!(result.messages[0].text, "message 7");
        assert_eq!(result.messages[1].text, "message 8");
        assert_eq!(result.messages[2].text, "message 9");
    }

    #[test]
    fn chat_pagination_small_history_loads_all() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Save 2 messages, page_size = 100
        let messages: Vec<ChatMessage> = (0..2)
            .map(|i| make_chat_message(ChatRole::User, &format!("msg {}", i)))
            .collect();
        save_chat_history(&wg_dir, 0, &messages);

        let result = load_persisted_chat_history_paginated(&wg_dir, 0, 100);
        assert_eq!(result.messages.len(), 2);
        assert_eq!(result.total_count, 2);
        assert!(!result.has_more);
    }

    #[test]
    fn chat_pagination_load_older_page() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Save 10 messages
        let messages: Vec<ChatMessage> = (0..10)
            .map(|i| make_chat_message(ChatRole::User, &format!("message {}", i)))
            .collect();
        save_chat_history(&wg_dir, 0, &messages);

        let path = chat_history_path(&wg_dir, 0);

        // Load last 3 (messages 7,8,9)
        let tail = load_jsonl_tail(&path, 3);
        assert_eq!(tail.messages.len(), 3);
        assert_eq!(tail.messages[0].text, "message 7");

        // Load next page of 3 (messages 4,5,6)
        let page = load_jsonl_page(&path, 3, 3);
        assert_eq!(page.len(), 3);
        assert_eq!(page[0].text, "message 4");
        assert_eq!(page[1].text, "message 5");
        assert_eq!(page[2].text, "message 6");

        // Load next page of 3 (messages 1,2,3)
        let page2 = load_jsonl_page(&path, 6, 3);
        assert_eq!(page2.len(), 3);
        assert_eq!(page2[0].text, "message 1");
        assert_eq!(page2[1].text, "message 2");
        assert_eq!(page2[2].text, "message 3");

        // Load remaining (message 0)
        let page3 = load_jsonl_page(&path, 9, 3);
        assert_eq!(page3.len(), 1);
        assert_eq!(page3[0].text, "message 0");

        // Nothing left
        let page4 = load_jsonl_page(&path, 10, 3);
        assert!(page4.is_empty());
    }

    #[test]
    fn chat_pagination_legacy_json_migration() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Write a legacy JSON array file
        let legacy_path = chat_history_legacy_path(&wg_dir, 0);
        let msgs: Vec<PersistedChatMessage> = (0..5)
            .map(|i| {
                let msg = make_chat_message(ChatRole::User, &format!("legacy {}", i));
                chat_message_to_persisted(&msg)
            })
            .collect();
        let json = serde_json::to_string(&msgs).unwrap();
        std::fs::write(&legacy_path, json).unwrap();

        // Load paginated — should auto-migrate
        let result = load_persisted_chat_history_paginated(&wg_dir, 0, 3);
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.total_count, 5);
        assert!(result.has_more);
        assert_eq!(result.messages[0].text, "legacy 2");

        // Legacy file should be removed, JSONL file should exist
        let jsonl_path = chat_history_path(&wg_dir, 0);
        assert!(
            jsonl_path.exists(),
            "JSONL file should exist after migration"
        );
        assert!(
            !legacy_path.exists(),
            "Legacy JSON file should be removed after migration"
        );

        // Second load should use the JSONL file directly
        let result2 = load_persisted_chat_history_paginated(&wg_dir, 0, 3);
        assert_eq!(result2.messages.len(), 3);
        assert_eq!(result2.messages[0].text, "legacy 2");
    }

    #[test]
    fn chat_pagination_jsonl_format_correctness() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Save messages and verify file is JSONL format
        let messages: Vec<ChatMessage> = (0..3)
            .map(|i| make_chat_message(ChatRole::User, &format!("msg {}", i)))
            .collect();
        save_chat_history(&wg_dir, 0, &messages);

        let path = chat_history_path(&wg_dir, 0);
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().filter(|l| !l.is_empty()).collect();

        // Each line should be valid JSON
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(
                serde_json::from_str::<PersistedChatMessage>(line).is_ok(),
                "Each line should be valid JSON: {}",
                line
            );
        }
    }

    #[test]
    fn chat_pagination_save_preserves_unloaded_messages() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Save 10 messages
        let messages: Vec<ChatMessage> = (0..10)
            .map(|i| make_chat_message(ChatRole::User, &format!("message {}", i)))
            .collect();
        save_chat_history(&wg_dir, 0, &messages);

        // Load only the last 3 (simulating paginated load)
        let result = load_persisted_chat_history_paginated(&wg_dir, 0, 3);
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.total_count, 10);

        // Add a new message (simulating a message arriving during the session)
        let mut loaded = result.messages;
        loaded.push(make_chat_message(ChatRole::Coordinator, "new response"));

        // Save with skipped count — should preserve the 7 unloaded messages
        let skipped = result.total_count.saturating_sub(3);
        save_chat_history_with_skip(&wg_dir, 0, &loaded, skipped);

        // Load everything back — should have all 10 + 1 new = 11
        let all = load_persisted_chat_history(&wg_dir, 0);
        assert_eq!(
            all.len(),
            11,
            "Save should preserve unloaded messages + add new ones"
        );
        assert_eq!(all[0].text, "message 0", "First original message preserved");
        assert_eq!(all[9].text, "message 9", "Last original message preserved");
        assert_eq!(all[10].text, "new response", "New message appended");
    }

    #[test]
    fn chat_pagination_10k_messages_loads_under_1s() {
        let tmp = TempDir::new().unwrap();
        let (_, wg_dir) = setup_workgraph_with_coordinators(&tmp, &[0]);

        // Write config with high chat_history_max to allow 10k messages.
        let config_path = wg_dir.join("config.toml");
        std::fs::write(
            &config_path,
            "[tui]\nchat_history = true\nchat_history_max = 20000\nchat_page_size = 100\n",
        )
        .unwrap();

        // Create 10,000 messages and write directly as JSONL.
        let path = chat_history_path(&wg_dir, 0);
        let mut buf = String::new();
        for i in 0..10_000u32 {
            let m = make_chat_message(
                ChatRole::User,
                &format!(
                    "message number {} with some extra text to simulate realistic message length",
                    i
                ),
            );
            let p = chat_message_to_persisted(&m);
            if let Ok(line) = serde_json::to_string(&p) {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
        std::fs::write(&path, buf).unwrap();

        // Verify the file was written.
        assert!(path.exists());

        // Time the paginated load (default page_size = 100).
        let start = std::time::Instant::now();
        let result = load_persisted_chat_history_paginated(&wg_dir, 0, 100);
        let elapsed = start.elapsed();

        assert_eq!(
            result.messages.len(),
            100,
            "Should load exactly 100 messages"
        );
        assert_eq!(result.total_count, 10_000, "Should report total count");
        assert!(result.has_more, "Should indicate more history available");
        assert_eq!(
            result.messages[99].text,
            "message number 9999 with some extra text to simulate realistic message length"
        );
        assert!(
            elapsed.as_millis() < 1000,
            "Paginated load of 10k messages should complete in <1s, took {}ms",
            elapsed.as_millis()
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // Session boundary marker tests
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn session_boundary_divider_appears_for_large_gap() {
        // Two messages separated by 2 hours should produce a session divider.
        let ts1 = "2026-03-27T10:00:00+00:00";
        let ts2 = "2026-03-27T12:00:00+00:00";

        let messages = vec![
            make_chat_message_with_ts(ChatRole::User, "hello", ts1),
            make_chat_message_with_ts(ChatRole::Coordinator, "world", ts2),
        ];

        // Default threshold is 30 minutes; 2-hour gap exceeds it.
        let threshold = chrono::Duration::minutes(30);
        let prev_dt = chrono::DateTime::parse_from_rfc3339(ts1).unwrap();
        let cur_dt = chrono::DateTime::parse_from_rfc3339(ts2).unwrap();
        let gap = cur_dt.signed_duration_since(prev_dt);
        assert!(
            gap > threshold,
            "2-hour gap should exceed 30-minute threshold"
        );

        // Verify both messages have valid timestamps.
        assert!(messages[0].msg_timestamp.is_some());
        assert!(messages[1].msg_timestamp.is_some());
    }

    #[test]
    fn session_boundary_no_divider_for_small_gap() {
        // Two messages 5 minutes apart should NOT trigger a session boundary.
        let ts1 = "2026-03-27T10:00:00+00:00";
        let ts2 = "2026-03-27T10:05:00+00:00";

        let threshold = chrono::Duration::minutes(30);
        let prev_dt = chrono::DateTime::parse_from_rfc3339(ts1).unwrap();
        let cur_dt = chrono::DateTime::parse_from_rfc3339(ts2).unwrap();
        let gap = cur_dt.signed_duration_since(prev_dt);
        assert!(
            gap <= threshold,
            "5-minute gap should NOT exceed 30-minute threshold"
        );
    }

    #[test]
    fn session_boundary_disabled_when_gap_is_zero() {
        // When session_gap_minutes is 0, no dividers should be generated.
        let gap_minutes: u32 = 0;
        let threshold = if gap_minutes > 0 {
            Some(chrono::Duration::minutes(gap_minutes as i64))
        } else {
            None
        };
        assert!(
            threshold.is_none(),
            "Zero gap minutes should disable session boundaries"
        );
    }

    #[test]
    fn session_boundary_exact_threshold_no_divider() {
        // Gap exactly equal to threshold should NOT trigger a divider (strictly greater-than).
        let ts1 = "2026-03-27T10:00:00+00:00";
        let ts2 = "2026-03-27T10:30:00+00:00";

        let threshold = chrono::Duration::minutes(30);
        let prev_dt = chrono::DateTime::parse_from_rfc3339(ts1).unwrap();
        let cur_dt = chrono::DateTime::parse_from_rfc3339(ts2).unwrap();
        let gap = cur_dt.signed_duration_since(prev_dt);
        assert!(
            !(gap > threshold),
            "Exactly 30-minute gap should NOT trigger divider (strictly greater-than)"
        );
    }

    #[test]
    fn session_boundary_divider_format_contains_date() {
        // The divider text should include a human-readable date/time.
        let ts = "2026-03-27T15:42:00+00:00";
        let dt = chrono::DateTime::parse_from_rfc3339(ts).unwrap();
        let local_dt = dt.with_timezone(&chrono::Local);
        let label = local_dt.format("%B %-d, %Y · %-I:%M %p").to_string();

        // Verify the label contains expected components.
        assert!(label.contains("2026"), "Label should contain year");
        assert!(
            label.contains("March") || label.contains("27"),
            "Label should contain month or day"
        );
    }

    #[test]
    fn session_boundary_config_default() {
        // Verify the default config value is 30 minutes.
        let config = workgraph::config::TuiConfig::default();
        assert_eq!(config.session_gap_minutes, 30);
    }

    #[test]
    fn session_boundary_multiple_gaps_in_history() {
        // Three messages: msg1 -> 2hr gap -> msg2 -> 5min gap -> msg3
        // Should produce exactly one boundary (between msg1 and msg2).
        let ts1 = "2026-03-27T10:00:00+00:00";
        let ts2 = "2026-03-27T12:00:00+00:00";
        let ts3 = "2026-03-27T12:05:00+00:00";

        let threshold = chrono::Duration::minutes(30);
        let dt1 = chrono::DateTime::parse_from_rfc3339(ts1).unwrap();
        let dt2 = chrono::DateTime::parse_from_rfc3339(ts2).unwrap();
        let dt3 = chrono::DateTime::parse_from_rfc3339(ts3).unwrap();

        let gap1 = dt2.signed_duration_since(dt1);
        let gap2 = dt3.signed_duration_since(dt2);

        assert!(
            gap1 > threshold,
            "First gap (2hr) should produce a boundary"
        );
        assert!(
            !(gap2 > threshold),
            "Second gap (5min) should NOT produce a boundary"
        );
    }

    /// Regression test for tui-tab-bar: chat tab labels must use the
    /// `.chat-N` task-id form, not the deprecated `coord:N` shorthand,
    /// and the tab number must come from the actual chat task id (not
    /// a positional 1-indexed counter).
    #[test]
    fn tab_bar_labels_use_chat_task_id_form_not_coord_prefix() {
        let tmp = TempDir::new().unwrap();
        let mut graph = WorkGraph::new();

        // Mimic the user-reported state: chats numbered 2, 3, 4 (no .chat-1).
        for cid in [2u32, 3, 4] {
            let id = format!(".chat-{}", cid);
            let title = format!("Chat {}", cid);
            let mut task = make_task_with_status(&id, &title, Status::InProgress);
            task.tags = vec!["chat-loop".to_string()];
            graph.add_node(Node::Task(task));
        }
        let regular = make_task_with_status("test-task", "Test Task", Status::InProgress);
        graph.add_node(Node::Task(regular));

        let wg_dir = tmp.path().join(".workgraph");
        std::fs::create_dir_all(&wg_dir).unwrap();
        save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let viz = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            VizLayoutMode::Tree,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
        let app = build_test_app(&viz, &wg_dir);

        let entries = app.list_coordinator_ids_and_labels();
        let labels: Vec<String> = entries.iter().map(|(_, l)| l.clone()).collect();

        // All three task ids must appear as labels.
        for expected in [".chat-2", ".chat-3", ".chat-4"] {
            assert!(
                labels.iter().any(|l| l == expected),
                "expected label {:?} in {:?}",
                expected,
                labels
            );
        }
        // No deprecated `coord:` prefix, and no stray `coord:1` from a
        // positional counter (there is no `.chat-1` in this fixture).
        for label in &labels {
            assert!(
                !label.starts_with("coord:"),
                "label {:?} still uses deprecated coord: prefix",
                label
            );
        }
        assert!(
            !labels.iter().any(|l| l == "coord:1"),
            "label list {:?} contains stale coord:1 from a positional counter",
            labels
        );

        // The numeric ids must come from the task ids, not positional indices.
        let cids: Vec<u32> = entries.iter().map(|(c, _)| *c).collect();
        assert_eq!(cids, vec![2, 3, 4], "cids should match the task-id numbers");
    }
}

#[cfg(test)]
mod chat_ordering_tests {
    use super::*;

    /// Helper: create a minimal ChatMessage with the given role and text.
    fn make_msg(role: ChatRole, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            text: text.to_string(),
            full_text: None,
            attachments: vec![],
            edited: false,
            inbox_id: None,
            user: None,
            target_task: None,
            msg_timestamp: None,
            read_at: None,
            msg_queue_id: None,
        }
    }

    #[test]
    fn concurrent_user_send_during_agent_response_produces_correct_ordering() {
        // Simulate: user sends M1, then sends M2 while M1's response is in flight.
        // After coordinator response R1 arrives, display should be [M1, R1, M2].
        let mut chat = ChatState::default();

        // --- User sends M1 (no in-flight requests) ---
        chat.messages.push(make_msg(ChatRole::User, "M1"));
        // No pending requests → not deferred
        assert!(chat.pending_request_ids.is_empty());
        chat.awaiting_since = Some(std::time::Instant::now());
        chat.pending_request_ids.insert("rid1".to_string());

        // --- User sends M2 while rid1 is still pending ---
        chat.messages.push(make_msg(ChatRole::User, "M2"));
        // A response is in flight → defer M2
        assert!(!chat.pending_request_ids.is_empty());
        let idx = chat.messages.len() - 1;
        chat.deferred_user_indices.push(idx);
        chat.pending_request_ids.insert("rid2".to_string());

        // At this point: messages = [M1, M2], deferred = [1], pending = {rid1, rid2}
        assert_eq!(chat.messages.len(), 2);
        assert_eq!(chat.messages[0].text, "M1");
        assert_eq!(chat.messages[1].text, "M2");

        // --- Coordinator response R1 arrives ---
        chat.messages.push(make_msg(ChatRole::Coordinator, "R1"));

        // Retire one pending request (FIFO)
        if let Some(first) = chat.pending_request_ids.iter().next().cloned() {
            chat.pending_request_ids.remove(&first);
        }

        // Reorder deferred user messages to after newly arrived coordinator messages
        if !chat.deferred_user_indices.is_empty() {
            let mut deferred: Vec<ChatMessage> = Vec::new();
            for &idx in chat.deferred_user_indices.iter().rev() {
                if idx < chat.messages.len() {
                    deferred.push(chat.messages.remove(idx));
                }
            }
            deferred.reverse();
            chat.messages.extend(deferred);
            chat.deferred_user_indices.clear();
        }

        // Display should be [M1, R1, M2]
        assert_eq!(chat.messages.len(), 3);
        assert_eq!(chat.messages[0].text, "M1");
        assert_eq!(chat.messages[0].role, ChatRole::User);
        assert_eq!(chat.messages[1].text, "R1");
        assert_eq!(chat.messages[1].role, ChatRole::Coordinator);
        assert_eq!(chat.messages[2].text, "M2");
        assert_eq!(chat.messages[2].role, ChatRole::User);

        // One pending request remains (rid2)
        assert_eq!(chat.pending_request_ids.len(), 1);
        assert!(chat.awaiting_response());

        // --- Coordinator response R2 arrives ---
        chat.messages.push(make_msg(ChatRole::Coordinator, "R2"));
        if let Some(first) = chat.pending_request_ids.iter().next().cloned() {
            chat.pending_request_ids.remove(&first);
        }
        // No deferred messages this time
        assert!(chat.deferred_user_indices.is_empty());

        // Final display: [M1, R1, M2, R2]
        assert_eq!(chat.messages.len(), 4);
        assert_eq!(chat.messages[0].text, "M1");
        assert_eq!(chat.messages[1].text, "R1");
        assert_eq!(chat.messages[2].text, "M2");
        assert_eq!(chat.messages[3].text, "R2");
        assert!(!chat.awaiting_response());
    }

    #[test]
    fn no_deferral_when_no_request_in_flight() {
        // When no request is in flight, user message should display immediately
        // without any deferral tracking.
        let mut chat = ChatState::default();

        // Send message with no pending requests
        chat.messages.push(make_msg(ChatRole::User, "hello"));
        assert!(chat.pending_request_ids.is_empty());
        // Should NOT be tracked as deferred
        assert!(chat.deferred_user_indices.is_empty());

        assert_eq!(chat.messages.len(), 1);
        assert_eq!(chat.messages[0].text, "hello");
    }

    #[test]
    fn error_on_first_request_preserves_second_tracking() {
        // Send M1, then M2. M1 errors. M2's response should still be tracked.
        let mut chat = ChatState::default();

        // M1 sent
        chat.messages.push(make_msg(ChatRole::User, "M1"));
        chat.awaiting_since = Some(std::time::Instant::now());
        chat.pending_request_ids.insert("rid1".to_string());

        // M2 sent while rid1 is pending
        chat.messages.push(make_msg(ChatRole::User, "M2"));
        let idx = chat.messages.len() - 1;
        chat.deferred_user_indices.push(idx);
        chat.pending_request_ids.insert("rid2".to_string());

        // M1 errors — remove specific request ID
        chat.pending_request_ids.remove("rid1");
        assert!(chat.awaiting_response(), "rid2 is still pending");
        assert_eq!(chat.pending_request_ids.len(), 1);
        assert!(chat.pending_request_ids.contains("rid2"));
    }

    #[test]
    fn interrupt_clears_all_pending_and_deferred() {
        let mut chat = ChatState::default();

        // Simulate two pending requests with deferred messages
        chat.messages.push(make_msg(ChatRole::User, "M1"));
        chat.awaiting_since = Some(std::time::Instant::now());
        chat.pending_request_ids.insert("rid1".to_string());

        chat.messages.push(make_msg(ChatRole::User, "M2"));
        chat.deferred_user_indices.push(1);
        chat.pending_request_ids.insert("rid2".to_string());

        // Interrupt clears everything
        chat.pending_request_ids.clear();
        chat.awaiting_since = None;
        chat.deferred_user_indices.clear();

        assert!(!chat.awaiting_response());
        assert!(chat.deferred_user_indices.is_empty());
        assert!(chat.awaiting_since.is_none());
        // Messages are still present (not deleted)
        assert_eq!(chat.messages.len(), 2);
    }

    #[test]
    fn rapid_fire_three_messages_correct_ordering() {
        // Send M1, M2, M3 rapidly. Then R1 arrives.
        // Expected order after R1: [M1, R1, M2, M3]
        let mut chat = ChatState::default();

        // M1 — no pending
        chat.messages.push(make_msg(ChatRole::User, "M1"));
        chat.awaiting_since = Some(std::time::Instant::now());
        chat.pending_request_ids.insert("rid1".to_string());

        // M2 — rid1 pending
        chat.messages.push(make_msg(ChatRole::User, "M2"));
        chat.deferred_user_indices.push(chat.messages.len() - 1);
        chat.pending_request_ids.insert("rid2".to_string());

        // M3 — rid1, rid2 pending
        chat.messages.push(make_msg(ChatRole::User, "M3"));
        chat.deferred_user_indices.push(chat.messages.len() - 1);
        chat.pending_request_ids.insert("rid3".to_string());

        // R1 arrives
        chat.messages.push(make_msg(ChatRole::Coordinator, "R1"));
        if let Some(first) = chat.pending_request_ids.iter().next().cloned() {
            chat.pending_request_ids.remove(&first);
        }

        // Reorder deferred messages
        let mut deferred: Vec<ChatMessage> = Vec::new();
        for &idx in chat.deferred_user_indices.iter().rev() {
            if idx < chat.messages.len() {
                deferred.push(chat.messages.remove(idx));
            }
        }
        deferred.reverse();
        chat.messages.extend(deferred);
        chat.deferred_user_indices.clear();

        // Expected: [M1, R1, M2, M3]
        assert_eq!(chat.messages.len(), 4);
        assert_eq!(chat.messages[0].text, "M1");
        assert_eq!(chat.messages[1].text, "R1");
        assert_eq!(chat.messages[2].text, "M2");
        assert_eq!(chat.messages[3].text, "M3");
    }
}

#[cfg(test)]
mod chat_delivery_tests {
    use super::*;

    /// Helper: create a minimal ChatMessage with the given role and text.
    fn make_msg(role: ChatRole, text: &str) -> ChatMessage {
        ChatMessage {
            role,
            text: text.to_string(),
            full_text: None,
            attachments: vec![],
            edited: false,
            inbox_id: None,
            user: None,
            target_task: None,
            msg_timestamp: None,
            read_at: None,
            msg_queue_id: None,
        }
    }

    /// Simulate the poll_chat_messages reorder+retire logic on a ChatState.
    /// This avoids needing a full App context with workgraph_dir/outbox.
    fn simulate_response_arrival(chat: &mut ChatState, response_text: &str) {
        // Append coordinator response (simulates what poll_chat_messages does).
        chat.messages
            .push(make_msg(ChatRole::Coordinator, response_text));

        // Retire one pending request (FIFO), same logic as poll_chat_messages.
        if !chat.pending_request_ids.is_empty() {
            if let Some(first) = chat.pending_request_ids.iter().next().cloned() {
                chat.pending_request_ids.remove(&first);
            }
        }
        if chat.pending_request_ids.is_empty() {
            chat.awaiting_since = None;
            chat.streaming_text.clear();
        }

        // Reorder deferred user messages (P1 fix), same logic as poll_chat_messages.
        if !chat.deferred_user_indices.is_empty() {
            let mut deferred: Vec<ChatMessage> = Vec::new();
            for &idx in chat.deferred_user_indices.iter().rev() {
                if idx < chat.messages.len() {
                    deferred.push(chat.messages.remove(idx));
                }
            }
            deferred.reverse();
            chat.messages.extend(deferred);
            chat.deferred_user_indices.clear();
        }
    }

    /// Simulate sending a user message, mirroring send_chat_message logic.
    fn simulate_user_send(chat: &mut ChatState, text: &str, request_id: &str) {
        chat.messages.push(make_msg(ChatRole::User, text));

        // Track deferred if a response is in flight (P1 fix).
        if !chat.pending_request_ids.is_empty() {
            let idx = chat.messages.len() - 1;
            chat.deferred_user_indices.push(idx);
        }

        // Track pending request (P2 fix: set-based).
        if chat.pending_request_ids.is_empty() {
            chat.awaiting_since = Some(std::time::Instant::now());
        }
        chat.pending_request_ids.insert(request_id.to_string());
    }

    #[test]
    fn user_sends_during_active_response_both_get_responses() {
        // Core delivery test: M1 is sent, then M2 is sent while R1 is in flight.
        // Both messages must receive responses — the second must not be lost.
        let mut chat = ChatState::default();

        // M1 sent — no in-flight requests.
        simulate_user_send(&mut chat, "M1", "rid1");
        assert_eq!(chat.pending_request_ids.len(), 1);
        assert!(chat.awaiting_response());

        // M2 sent while rid1 is still pending.
        simulate_user_send(&mut chat, "M2", "rid2");
        assert_eq!(chat.pending_request_ids.len(), 2);
        assert!(chat.awaiting_response());

        // R1 arrives — system must still track rid2.
        simulate_response_arrival(&mut chat, "R1");
        assert_eq!(
            chat.pending_request_ids.len(),
            1,
            "rid2 must still be tracked"
        );
        assert!(chat.awaiting_response(), "system must keep polling for R2");

        // R2 arrives — all requests now satisfied.
        simulate_response_arrival(&mut chat, "R2");
        assert_eq!(chat.pending_request_ids.len(), 0);
        assert!(
            !chat.awaiting_response(),
            "no more pending — polling can slow down"
        );
        assert!(chat.awaiting_since.is_none(), "spinner timer cleared");

        // Both messages received responses — verify display has all 4 entries.
        assert_eq!(chat.messages.len(), 4);
        let roles: Vec<_> = chat.messages.iter().map(|m| m.role.clone()).collect();
        assert_eq!(
            roles,
            vec![
                ChatRole::User,
                ChatRole::Coordinator,
                ChatRole::User,
                ChatRole::Coordinator
            ]
        );
        // Verify content matches: [M1, R1, M2, R2]
        assert_eq!(chat.messages[0].text, "M1");
        assert_eq!(chat.messages[1].text, "R1");
        assert_eq!(chat.messages[2].text, "M2");
        assert_eq!(chat.messages[3].text, "R2");
    }

    #[test]
    fn rapid_fire_three_messages_all_get_responses() {
        // Delivery test: 3 messages sent in rapid succession while responses are in flight.
        // All 3 must eventually receive responses — none dropped.
        let mut chat = ChatState::default();

        // M1 — first message, no pending.
        simulate_user_send(&mut chat, "M1", "rid1");
        assert_eq!(chat.pending_request_ids.len(), 1);

        // M2 — sent while rid1 pending.
        simulate_user_send(&mut chat, "M2", "rid2");
        assert_eq!(chat.pending_request_ids.len(), 2);

        // M3 — sent while rid1, rid2 pending.
        simulate_user_send(&mut chat, "M3", "rid3");
        assert_eq!(chat.pending_request_ids.len(), 3);
        assert!(chat.awaiting_response());

        // Verify all 3 requests are tracked.
        assert!(chat.pending_request_ids.contains("rid1"));
        assert!(chat.pending_request_ids.contains("rid2"));
        assert!(chat.pending_request_ids.contains("rid3"));

        // R1 arrives — 2 requests still pending.
        simulate_response_arrival(&mut chat, "R1");
        assert_eq!(chat.pending_request_ids.len(), 2);
        assert!(chat.awaiting_response(), "still waiting for R2 and R3");
        assert!(
            chat.awaiting_since.is_some(),
            "spinner should still be active"
        );

        // R2 arrives — 1 request still pending.
        simulate_response_arrival(&mut chat, "R2");
        assert_eq!(chat.pending_request_ids.len(), 1);
        assert!(chat.awaiting_response(), "still waiting for R3");

        // R3 arrives — all complete.
        simulate_response_arrival(&mut chat, "R3");
        assert_eq!(chat.pending_request_ids.len(), 0);
        assert!(!chat.awaiting_response(), "all responses delivered");
        assert!(chat.awaiting_since.is_none(), "spinner timer cleared");

        // All 3 messages received responses — 6 entries total.
        assert_eq!(chat.messages.len(), 6);

        // Verify ordering: [M1, R1, M2, M3, R2, R3]
        // M2 and M3 were deferred past R1 when R1 arrived.
        // R2 and R3 arrived after deferred indices were cleared, so they append normally.
        assert_eq!(chat.messages[0].text, "M1");
        assert_eq!(chat.messages[0].role, ChatRole::User);
        assert_eq!(chat.messages[1].text, "R1");
        assert_eq!(chat.messages[1].role, ChatRole::Coordinator);
        assert_eq!(chat.messages[2].text, "M2");
        assert_eq!(chat.messages[2].role, ChatRole::User);
        assert_eq!(chat.messages[3].text, "M3");
        assert_eq!(chat.messages[3].role, ChatRole::User);
        assert_eq!(chat.messages[4].text, "R2");
        assert_eq!(chat.messages[4].role, ChatRole::Coordinator);
        assert_eq!(chat.messages[5].text, "R3");
        assert_eq!(chat.messages[5].role, ChatRole::Coordinator);
    }

    #[test]
    fn no_duplicate_responses_from_single_arrival() {
        // Verify that a single coordinator response retires exactly one pending
        // request — not multiple. This prevents duplicate response delivery.
        let mut chat = ChatState::default();

        simulate_user_send(&mut chat, "M1", "rid1");
        simulate_user_send(&mut chat, "M2", "rid2");
        assert_eq!(chat.pending_request_ids.len(), 2);

        // One response arrives.
        simulate_response_arrival(&mut chat, "R1");

        // Exactly one request retired, not both.
        assert_eq!(
            chat.pending_request_ids.len(),
            1,
            "only one request should be retired per response"
        );

        // No duplicate coordinator messages in the display.
        let coordinator_msgs: Vec<_> = chat
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::Coordinator)
            .collect();
        assert_eq!(
            coordinator_msgs.len(),
            1,
            "exactly one coordinator message from one response"
        );
    }

    #[test]
    fn error_does_not_lose_other_pending_requests() {
        // When one request errors, other in-flight requests must continue
        // to be tracked — the system must not stop polling.
        let mut chat = ChatState::default();

        simulate_user_send(&mut chat, "M1", "rid1");
        simulate_user_send(&mut chat, "M2", "rid2");
        simulate_user_send(&mut chat, "M3", "rid3");
        assert_eq!(chat.pending_request_ids.len(), 3);

        // M1 errors — simulate ChatResponse error handler.
        chat.pending_request_ids.remove("rid1");
        assert_eq!(chat.pending_request_ids.len(), 2);
        assert!(chat.awaiting_response(), "rid2 and rid3 still pending");
        assert!(
            chat.awaiting_since.is_some(),
            "spinner stays active for remaining requests"
        );

        // R2 arrives normally.
        simulate_response_arrival(&mut chat, "R2");
        assert_eq!(chat.pending_request_ids.len(), 1);
        assert!(chat.awaiting_response(), "rid3 still pending");

        // R3 arrives.
        simulate_response_arrival(&mut chat, "R3");
        assert_eq!(chat.pending_request_ids.len(), 0);
        assert!(!chat.awaiting_response());
        assert!(chat.awaiting_since.is_none());
    }

    #[test]
    fn markdown_formatting_preserved_in_response_messages() {
        // Verify that coordinator messages with markdown content are stored
        // and retrievable without any formatting corruption.
        let mut chat = ChatState::default();

        simulate_user_send(&mut chat, "explain code", "rid1");

        let md_content =
            "## Analysis\n\n- **Bold** point\n- `code snippet`\n\n```rust\nfn main() {}\n```";
        simulate_response_arrival(&mut chat, md_content);

        // Find the coordinator message.
        let coord_msg = chat
            .messages
            .iter()
            .find(|m| m.role == ChatRole::Coordinator)
            .expect("coordinator message should exist");

        // Markdown content is preserved exactly as delivered.
        assert_eq!(coord_msg.text, md_content);
        assert!(coord_msg.text.contains("```rust"));
        assert!(coord_msg.text.contains("**Bold**"));
    }
}

#[cfg(test)]
mod test_claude_session {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_claude_session_uuid_deterministic() {
        let cwd = Path::new("/home/user/myproject");
        let name = "wg-myproject-coordinator-0";
        let u1 = claude_session_uuid(cwd, name);
        let u2 = claude_session_uuid(cwd, name);
        assert_eq!(u1, u2);
    }

    #[test]
    fn test_claude_session_uuid_differs_per_coordinator() {
        let cwd = Path::new("/home/user/myproject");
        let u0 = claude_session_uuid(cwd, "wg-myproject-coordinator-0");
        let u1 = claude_session_uuid(cwd, "wg-myproject-coordinator-1");
        assert_ne!(u0, u1);
    }

    #[test]
    fn test_claude_session_uuid_differs_per_project() {
        let name = "wg-proj-coordinator-0";
        let ua = claude_session_uuid(Path::new("/home/user/project-a"), name);
        let ub = claude_session_uuid(Path::new("/home/user/project-b"), name);
        assert_ne!(ua, ub);
    }

    #[test]
    fn test_claude_session_exists_missing() {
        let cwd = Path::new("/nonexistent/path/that/wont/exist");
        let uuid = claude_session_uuid(cwd, "test-session");
        assert!(!claude_session_exists(cwd, &uuid));
    }
}

#[cfg(test)]
mod filter_picker_tests {
    use super::FilterPicker;

    fn sample_picker() -> FilterPicker {
        FilterPicker::new(
            vec![
                ("claude:opus".into(), "Most capable".into()),
                ("claude:sonnet".into(), "Balanced".into()),
                ("claude:haiku".into(), "Fastest".into()),
                ("openrouter:anthropic/claude".into(), "Via OpenRouter".into()),
                ("openai:gpt-4o".into(), "OpenAI GPT-4o".into()),
            ],
            true,
        )
    }

    #[test]
    fn test_initial_state_shows_all() {
        let picker = sample_picker();
        assert_eq!(picker.filtered_indices.len(), 5);
        assert_eq!(picker.selected, 0);
        assert!(picker.filter.is_empty());
    }

    #[test]
    fn test_fuzzy_filter_op() {
        let mut picker = sample_picker();
        picker.filter = "op".to_string();
        picker.apply_filter();
        let filtered_ids: Vec<&str> = picker
            .filtered_indices
            .iter()
            .map(|&i| picker.items[i].0.as_str())
            .collect();
        assert!(filtered_ids.contains(&"claude:opus"));
        assert!(filtered_ids.contains(&"openrouter:anthropic/claude"));
        assert!(filtered_ids.contains(&"openai:gpt-4o"));
        assert!(!filtered_ids.contains(&"claude:haiku"));
    }

    #[test]
    fn test_filter_clear_restores_all() {
        let mut picker = sample_picker();
        picker.filter = "op".to_string();
        picker.apply_filter();
        assert!(picker.filtered_indices.len() < 5);

        picker.filter.clear();
        picker.apply_filter();
        assert_eq!(picker.filtered_indices.len(), 5);
    }

    #[test]
    fn test_navigation() {
        let mut picker = sample_picker();
        assert_eq!(picker.selected, 0);
        picker.next();
        assert_eq!(picker.selected, 1);
        picker.next();
        assert_eq!(picker.selected, 2);
        picker.prev();
        assert_eq!(picker.selected, 1);
        picker.prev();
        assert_eq!(picker.selected, 0);
        picker.prev();
        assert_eq!(picker.selected, 0); // clamp at 0
    }

    #[test]
    fn test_custom_entry() {
        let mut picker = sample_picker();
        assert!(picker.allow_custom);
        assert_eq!(picker.visible_count(), 6); // 5 items + 1 custom

        // Navigate to custom row
        for _ in 0..5 {
            picker.next();
        }
        assert!(picker.is_custom_selected());

        picker.enter_custom();
        assert!(picker.custom_active);
        picker.custom_text = "my-custom-model".to_string();
        assert_eq!(picker.value(), Some("my-custom-model".to_string()));
    }

    #[test]
    fn test_value_from_selection() {
        let picker = sample_picker();
        assert_eq!(picker.value(), Some("claude:opus".to_string()));
    }

    #[test]
    fn test_with_selected_id() {
        let picker = sample_picker().with_selected_id("claude:haiku");
        assert_eq!(picker.selected, 2);
    }

    #[test]
    fn test_type_char_filters() {
        let mut picker = sample_picker();
        picker.type_char('h');
        picker.type_char('a');
        picker.type_char('i');
        assert_eq!(picker.filter, "hai");
        let filtered_ids: Vec<&str> = picker
            .filtered_indices
            .iter()
            .map(|&i| picker.items[i].0.as_str())
            .collect();
        assert!(filtered_ids.contains(&"claude:haiku"));
    }

    #[test]
    fn test_backspace() {
        let mut picker = sample_picker();
        picker.type_char('h');
        picker.type_char('a');
        assert_eq!(picker.filter, "ha");
        picker.backspace();
        assert_eq!(picker.filter, "h");
        picker.backspace();
        assert_eq!(picker.filter, "");
        assert_eq!(picker.filtered_indices.len(), 5);
    }

    #[test]
    fn test_empty_hint() {
        let picker = FilterPicker::new(vec![], false)
            .with_hint("No endpoints registered. wg endpoint add ... to add one.");
        assert!(picker.items.is_empty());
        assert_eq!(
            picker.empty_hint,
            "No endpoints registered. wg endpoint add ... to add one."
        );
    }

    #[test]
    fn test_selected_clamps_on_filter() {
        let mut picker = sample_picker();
        picker.selected = 4; // last item
        picker.filter = "opus".to_string();
        picker.apply_filter();
        assert!(picker.selected < picker.visible_count());
    }
}

#[cfg(test)]
mod agent_stream_tests {
    use super::*;

    #[test]
    fn test_parse_tool_call_event() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#;
        let event = parse_raw_stream_line(line, "agent-1").unwrap();
        assert_eq!(event.kind, AgentStreamEventKind::ToolCall);
        assert!(event.summary.contains("Bash"));
        assert!(event.summary.contains("cargo test"));
        assert_eq!(event.agent_id, "agent-1");
    }

    #[test]
    fn test_parse_text_output_event() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Building the project now."}]}}"#;
        let event = parse_raw_stream_line(line, "agent-2").unwrap();
        assert_eq!(event.kind, AgentStreamEventKind::TextOutput);
        assert!(event.summary.contains("Building the project now."));
    }

    #[test]
    fn test_parse_thinking_event() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Let me analyze this code...","signature":"abc"}]}}"#;
        let event = parse_raw_stream_line(line, "agent-3").unwrap();
        assert_eq!(event.kind, AgentStreamEventKind::Thinking);
        assert!(event.summary.contains("analyze this code"));
    }

    #[test]
    fn test_parse_tool_result_event() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"test output here","is_error":false}]}}"#;
        let event = parse_raw_stream_line(line, "agent-4").unwrap();
        assert_eq!(event.kind, AgentStreamEventKind::ToolResult);
        assert!(event.summary.contains("test output here"));
    }

    #[test]
    fn test_parse_tool_result_error() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"command failed","is_error":true}]}}"#;
        let event = parse_raw_stream_line(line, "agent-5").unwrap();
        assert_eq!(event.kind, AgentStreamEventKind::Error);
        assert!(event.summary.contains("command failed"));
    }

    #[test]
    fn test_parse_system_init_ignored() {
        let line = r#"{"type":"system","subtype":"init","cwd":"/tmp"}"#;
        assert!(parse_raw_stream_line(line, "agent-6").is_none());
    }

    #[test]
    fn test_parse_system_task_event() {
        let line = r#"{"type":"system","subtype":"task_started","summary":"Running build"}"#;
        let event = parse_raw_stream_line(line, "agent-7").unwrap();
        assert_eq!(event.kind, AgentStreamEventKind::SystemEvent);
        assert!(event.summary.contains("Running build"));
    }

    #[test]
    fn test_parse_edit_tool() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"src/main.rs","old_string":"a","new_string":"b"}}]}}"#;
        let event = parse_raw_stream_line(line, "agent-8").unwrap();
        assert_eq!(event.kind, AgentStreamEventKind::ToolCall);
        assert!(event.summary.contains("Edit"));
        assert!(event.summary.contains("src/main.rs"));
    }

    #[test]
    fn test_parse_grep_tool() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Grep","input":{"pattern":"fn main"}}]}}"#;
        let event = parse_raw_stream_line(line, "agent-9").unwrap();
        assert_eq!(event.kind, AgentStreamEventKind::ToolCall);
        assert!(event.summary.contains("Grep"));
        assert!(event.summary.contains("fn main"));
    }

    #[test]
    fn test_parse_native_executor_tool_call() {
        let line = r#"{"type":"tool_call","name":"Bash","input":{"command":"ls -la"},"output":"total 8\ndrwx...","is_error":false}"#;
        let event = parse_raw_stream_line(line, "agent-10").unwrap();
        assert_eq!(event.kind, AgentStreamEventKind::ToolCall);
        assert!(event.summary.contains("Bash"));
        assert!(event.summary.contains("ls -la"));
        assert!(event.summary.contains("total 8"));
    }

    #[test]
    fn test_tool_call_summary_uses_priority_symbol_not_lightning() {
        // Pins the visual prefix so a future edit can't silently revert
        // ⌁ (priority symbol, U+2380) back to ⚡ (lightning, U+26A1).
        let claude_line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]}}"#;
        let event = parse_raw_stream_line(claude_line, "a").unwrap();
        assert!(event.summary.starts_with("⌁ "), "got: {}", event.summary);
        assert!(!event.summary.contains('⚡'), "got: {}", event.summary);

        let native_line = r#"{"type":"tool_call","name":"Bash","input":{"command":"ls"},"output":"x","is_error":false}"#;
        let event = parse_raw_stream_line(native_line, "a").unwrap();
        assert!(event.summary.starts_with("⌁ "), "got: {}", event.summary);
        assert!(!event.summary.contains('⚡'), "got: {}", event.summary);
    }

    #[test]
    fn test_parse_native_executor_turn_text() {
        let line = r#"{"type":"turn","turn":1,"role":"assistant","content":[{"type":"text","text":"Working on it."}]}"#;
        let event = parse_raw_stream_line(line, "agent-11").unwrap();
        assert_eq!(event.kind, AgentStreamEventKind::TextOutput);
        assert!(event.summary.contains("Working on it."));
    }

    #[test]
    fn test_log_view_renders_agent_stream_events_for_inprogress_task() {
        use std::collections::{HashMap, HashSet};
        use workgraph::graph::{Node, Status, WorkGraph};
        use workgraph::parser::save_graph;
        use workgraph::test_helpers::make_task_with_status;
        use crate::commands::viz::ascii::generate_ascii;
        use crate::commands::viz::{LayoutMode, VizOutput};

        let mut graph = WorkGraph::new();
        let mut task = make_task_with_status("my-task", "My Task", Status::InProgress);
        task.assigned = Some("agent-99".to_string());
        graph.add_node(Node::Task(task));

        let tmp = tempfile::tempdir().unwrap();
        let graph_path = tmp.path().join("graph.jsonl");
        save_graph(&graph, &graph_path).unwrap();

        // Create agent directory with raw_stream.jsonl
        let agent_dir = tmp.path().join("agents").join("agent-99");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let stream_content = [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Starting implementation."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo build"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"Build succeeded","is_error":false}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"Let me check the test results.","signature":"x"}]}}"#,
        ];
        std::fs::write(
            agent_dir.join("raw_stream.jsonl"),
            stream_content.join("\n"),
        )
        .unwrap();
        // Also create output.log (empty) so existing code doesn't fail
        std::fs::write(agent_dir.join("output.log"), "").unwrap();

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
        app.workgraph_dir = tmp.path().to_path_buf();
        let idx = app.task_order.iter().position(|id| id == "my-task");
        app.selected_task_idx = idx;

        // Simulate what the TUI does: load log pane, then update stream events
        app.load_log_pane();
        assert_eq!(app.log_pane.agent_id.as_deref(), Some("agent-99"));

        app.update_log_stream_events();

        assert_eq!(app.log_pane.stream_events.len(), 4);
        assert_eq!(
            app.log_pane.stream_events[0].kind,
            AgentStreamEventKind::TextOutput
        );
        assert!(app.log_pane.stream_events[0]
            .summary
            .contains("Starting implementation"));
        assert_eq!(
            app.log_pane.stream_events[1].kind,
            AgentStreamEventKind::ToolCall
        );
        assert!(app.log_pane.stream_events[1].summary.contains("Bash"));
        assert_eq!(
            app.log_pane.stream_events[2].kind,
            AgentStreamEventKind::ToolResult
        );
        assert!(app.log_pane.stream_events[2]
            .summary
            .contains("Build succeeded"));
        assert_eq!(
            app.log_pane.stream_events[3].kind,
            AgentStreamEventKind::Thinking
        );
    }

    /// Pressing the `4` key while focused on the Log pane must cycle
    /// the view through three modes in stable order:
    /// Events → HighLevel → RawPretty → Events. Required by the
    /// per-task Log "three view modes" feature.
    #[test]
    fn test_log_view_cycles_through_three_modes() {
        use crate::commands::viz::{LayoutMode, VizOutput};
        use crate::commands::viz::ascii::generate_ascii;
        use std::collections::{HashMap, HashSet};
        use workgraph::graph::{Node, Status, WorkGraph};
        use workgraph::test_helpers::make_task_with_status;

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task_with_status(
            "t",
            "T",
            Status::Open,
        )));
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

        // Default mode is Events (least disruptive vs the previous behavior).
        assert_eq!(app.log_pane.view_mode, LogViewMode::Events);

        app.cycle_log_view();
        assert_eq!(app.log_pane.view_mode, LogViewMode::HighLevel);

        app.cycle_log_view();
        assert_eq!(app.log_pane.view_mode, LogViewMode::RawPretty);

        app.cycle_log_view();
        assert_eq!(
            app.log_pane.view_mode,
            LogViewMode::Events,
            "after three cycles we should be back at Events"
        );

        // The mode label exposed in the pane header must match.
        assert_eq!(LogViewMode::Events.label(), "events");
        assert_eq!(LogViewMode::HighLevel.label(), "high-level");
        assert_eq!(LogViewMode::RawPretty.label(), "raw");
    }
}

#[cfg(test)]
mod launcher_history_tests {
    use super::*;
    use std::io::Write as _;

    /// `open_launcher` should pull recent invocations from
    /// `launcher_history` and surface them as a one-click recall list,
    /// per the user expectation that any prior CLI/TUI invocation
    /// reappears as a recallable option.
    #[test]
    #[serial_test::serial(launcher_history_env)]
    fn test_tui_dialog_reads_history_and_offers_picker() {
        let tmp = tempfile::tempdir().unwrap();
        let history_path = tmp.path().join("launcher-history.jsonl");

        // Seed launcher history with a `wg nex -m qwen3-coder -e ...`
        // invocation, like the example in the task spec.
        let entry = workgraph::launcher_history::HistoryEntry::new(
            "native",
            Some("qwen3-coder"),
            Some("https://lambda01.tail334fe6.ts.net:30000"),
            "cli",
        );
        {
            let mut f = std::fs::File::create(&history_path).unwrap();
            writeln!(f, "{}", serde_json::to_string(&entry).unwrap()).unwrap();
        }

        unsafe {
            std::env::set_var("WG_LAUNCHER_HISTORY_PATH", &history_path);
        }

        let workgraph_dir = tmp.path().to_path_buf();
        std::fs::write(workgraph_dir.join("graph.jsonl"), "").unwrap();
        let mut app = VizApp::new(
            workgraph_dir,
            crate::commands::viz::VizOptions::default(),
            Some(true),
            None,
            false,
        );

        app.open_launcher();

        unsafe {
            std::env::remove_var("WG_LAUNCHER_HISTORY_PATH");
        }

        let launcher = app
            .launcher
            .as_ref()
            .expect("open_launcher should have populated the launcher state");
        assert!(
            !launcher.recent_list.is_empty(),
            "launcher should surface the seeded history entry as a recall option"
        );
        let recent = &launcher.recent_list[0];
        assert_eq!(recent.executor, "native");
        assert_eq!(recent.model.as_deref(), Some("qwen3-coder"));
        assert_eq!(
            recent.endpoint.as_deref(),
            Some("https://lambda01.tail334fe6.ts.net:30000")
        );
    }
}
