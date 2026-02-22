use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use workgraph::graph::{Status, Task, WorkGraph};
use workgraph::parser::load_graph;
use workgraph::{AgentEntry, AgentRegistry, AgentStatus};

use super::graph_layout::DagLayout;

/// How long a recently-changed item stays highlighted (seconds)
const HIGHLIGHT_DURATION: Duration = Duration::from_secs(3);

/// Which panel is currently focused
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Tasks,
    Agents,
}

/// A display-ready task entry with precomputed sort order
#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub assigned: Option<String>,
}

impl TaskEntry {
    /// Sort key: in-progress=0, open(ready)=1, failed=2, blocked=3, done=4, abandoned=5
    fn sort_key(&self) -> u8 {
        match self.status {
            Status::InProgress => 0,
            Status::Open => 1,
            Status::Failed => 2,
            Status::Blocked => 3,
            Status::Done => 4,
            Status::Abandoned => 5,
        }
    }
}

/// A snapshot of agent info for display, with PID liveness resolved
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub id: String,
    pub task_id: String,
    pub executor: String,
    pub pid: u32,
    pub uptime: String,
    pub status: AgentStatus,
    /// Path to the agent's output log file
    pub output_file: String,
}

impl AgentInfo {
    fn from_entry(entry: &AgentEntry) -> Self {
        let process_alive = is_process_alive(entry.pid);
        let effective_status = if entry.is_alive() && !process_alive {
            AgentStatus::Dead
        } else {
            entry.status
        };
        Self {
            id: entry.id.clone(),
            task_id: entry.task_id.clone(),
            executor: entry.executor.clone(),
            pid: entry.pid,
            uptime: entry.uptime_human(),
            status: effective_status,
            output_file: entry.output_file.clone(),
        }
    }

    pub fn is_alive(&self) -> bool {
        matches!(
            self.status,
            AgentStatus::Starting | AgentStatus::Working | AgentStatus::Idle
        )
    }

    pub fn is_dead(&self) -> bool {
        matches!(self.status, AgentStatus::Dead | AgentStatus::Failed)
    }
}

use crate::commands::is_process_alive;

/// Which view is currently active
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum View {
    /// Main dashboard with task and agent panels
    Dashboard,
    /// Log viewer for a specific agent
    LogView,
    /// Graph explorer showing dependency graph
    GraphExplorer,
}

/// State for the agent log viewer
pub struct LogViewer {
    /// The agent whose log we're viewing
    pub agent: AgentInfo,
    /// All lines read from the log file so far
    pub lines: Vec<String>,
    /// Current scroll offset (line index at the top of the viewport)
    pub scroll_offset: usize,
    /// Whether auto-scroll to bottom is active
    pub auto_scroll: bool,
    /// File byte position we've read up to (for incremental tailing)
    file_pos: u64,
    /// Last time we polled the file
    last_poll: Instant,
}

/// How often to poll the log file for new content
const LOG_POLL_INTERVAL: Duration = Duration::from_millis(500);

impl LogViewer {
    pub fn new(agent: AgentInfo) -> Self {
        let mut viewer = Self {
            agent,
            lines: Vec::new(),
            scroll_offset: 0,
            auto_scroll: true,
            file_pos: 0,
            last_poll: Instant::now() - LOG_POLL_INTERVAL, // force immediate first read
        };
        viewer.poll_file();
        viewer
    }

    /// Read any new content from the log file
    pub fn poll_file(&mut self) {
        if self.last_poll.elapsed() < LOG_POLL_INTERVAL {
            return;
        }
        self.last_poll = Instant::now();

        let path = &self.agent.output_file;
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => {
                if self.lines.is_empty() {
                    self.lines.push(format!("(Cannot open log file: {})", path));
                }
                return;
            }
        };

        let mut reader = BufReader::new(file);
        if reader.seek(SeekFrom::Start(self.file_pos)).is_err() {
            return;
        }

        let mut buf = String::new();
        loop {
            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    self.file_pos += n as u64;
                    // Strip trailing newline
                    let line = buf
                        .trim_end_matches('\n')
                        .trim_end_matches('\r')
                        .to_string();
                    self.lines.push(line);
                }
                Err(_) => break,
            }
        }
    }

    /// Scroll up by one line, disabling auto-scroll
    pub fn scroll_up(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
            self.auto_scroll = false;
        }
    }

    /// Scroll down by one line
    pub fn scroll_down(&mut self, viewport_height: usize) {
        let max_offset = self.lines.len().saturating_sub(viewport_height);
        if self.scroll_offset < max_offset {
            self.scroll_offset += 1;
        }
        // Re-enable auto-scroll if we've scrolled to the bottom
        if self.scroll_offset >= max_offset {
            self.auto_scroll = true;
        }
    }

    /// Page up (half viewport)
    pub fn page_up(&mut self, viewport_height: usize) {
        let jump = viewport_height / 2;
        self.scroll_offset = self.scroll_offset.saturating_sub(jump);
        self.auto_scroll = false;
    }

    /// Page down (half viewport)
    pub fn page_down(&mut self, viewport_height: usize) {
        let jump = viewport_height / 2;
        let max_offset = self.lines.len().saturating_sub(viewport_height);
        self.scroll_offset = (self.scroll_offset + jump).min(max_offset);
        if self.scroll_offset >= max_offset {
            self.auto_scroll = true;
        }
    }

    /// Apply auto-scroll: set offset so the last line is visible
    pub fn apply_auto_scroll(&mut self, viewport_height: usize) {
        if self.auto_scroll {
            self.scroll_offset = self.lines.len().saturating_sub(viewport_height);
        }
    }
}

/// A flattened row in the graph explorer tree view
#[derive(Debug, Clone)]
pub struct GraphRow {
    pub task_id: String,
    pub title: String,
    pub status: Status,
    pub assigned: Option<String>,
    /// Indentation depth (0 = root)
    pub depth: usize,
    /// Whether this node's children are collapsed
    pub collapsed: bool,
    /// Whether this node is on the critical path
    pub critical: bool,
    /// Back-reference marker: if this task has multiple blockers, shows the
    /// primary parent it's listed under (empty if it's the canonical position)
    pub back_ref: Option<String>,
    /// Number of active (alive) agents currently working on this task
    pub active_agent_count: usize,
    /// IDs of active agents on this task (for display)
    pub active_agent_ids: Vec<String>,
}

/// Active agent info for a task in the graph explorer
#[derive(Debug, Clone)]
pub struct TaskAgentInfo {
    pub agent_ids: Vec<String>,
    pub count: usize,
}

/// Which display mode the graph explorer is using
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphViewMode {
    /// Indented tree list (original)
    Tree,
    /// Visual graph layout with boxes and edges
    Dag,
}

/// State for the graph explorer view
pub struct GraphExplorer {
    /// Flattened rows for display
    pub rows: Vec<GraphRow>,
    /// Selected row index
    pub selected: usize,
    /// Set of collapsed task IDs
    pub collapsed_ids: HashSet<String>,
    /// Whether we're showing a detail overlay for the selected task
    pub show_detail: bool,
    /// Scroll offset for the detail view
    pub detail_scroll: usize,
    /// Full task data for detail view (loaded on demand)
    pub detail_task: Option<Task>,
    /// Map from task_id to active agent info (only alive agents)
    pub agent_map: HashMap<String, TaskAgentInfo>,
    /// Indices of rows that have active agents (for 'a' cycling)
    pub agent_active_indices: Vec<usize>,
    /// Current view mode (tree or graph)
    pub view_mode: GraphViewMode,
    /// Cached graph layout (computed on rebuild when in graph mode)
    pub graph_layout: Option<DagLayout>,
    /// Selected node index in graph mode
    pub dag_selected: usize,
    /// Horizontal scroll offset for graph view
    pub dag_scroll_x: usize,
    /// Vertical scroll offset for graph view
    pub dag_scroll_y: usize,
}

impl GraphExplorer {
    pub fn new(workgraph_dir: &std::path::Path) -> Self {
        let mut explorer = Self {
            rows: Vec::new(),
            selected: 0,
            collapsed_ids: HashSet::new(),
            show_detail: false,
            detail_scroll: 0,
            detail_task: None,
            agent_map: HashMap::new(),
            agent_active_indices: Vec::new(),
            view_mode: GraphViewMode::Tree,
            graph_layout: None,
            dag_selected: 0,
            dag_scroll_x: 0,
            dag_scroll_y: 0,
        };
        explorer.rebuild(workgraph_dir);
        explorer
    }

    /// Load active agent mapping from the registry
    fn load_agent_map(workgraph_dir: &std::path::Path) -> HashMap<String, TaskAgentInfo> {
        let mut map: HashMap<String, TaskAgentInfo> = HashMap::new();
        let registry = match AgentRegistry::load(workgraph_dir) {
            Ok(r) => r,
            Err(_) => return map,
        };

        for entry in registry.list_agents() {
            let process_alive = is_process_alive(entry.pid);
            let effectively_alive = entry.is_alive() && process_alive;
            if effectively_alive {
                let info = map
                    .entry(entry.task_id.clone())
                    .or_insert_with(|| TaskAgentInfo {
                        agent_ids: Vec::new(),
                        count: 0,
                    });
                info.agent_ids.push(entry.id.clone());
                info.count += 1;
            }
        }
        map
    }

    /// Rebuild the flattened tree from the graph data
    pub fn rebuild(&mut self, workgraph_dir: &std::path::Path) {
        let graph_path = workgraph_dir.join("graph.jsonl");
        let graph = match load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => {
                self.rows.clear();
                self.graph_layout = None;
                return;
            }
        };

        let critical_ids = compute_critical_path(&graph);
        let agent_map = Self::load_agent_map(workgraph_dir);
        let rows = build_graph_tree(&graph, &self.collapsed_ids, &critical_ids, &agent_map);

        // Preserve selection by task ID
        let prev_id = self.rows.get(self.selected).map(|r| r.task_id.clone());
        self.rows = rows;
        if let Some(ref id) = prev_id
            && let Some(pos) = self
                .rows
                .iter()
                .position(|r| r.task_id == *id && r.back_ref.is_none())
        {
            self.selected = pos;
        }
        if !self.rows.is_empty() {
            self.selected = self.selected.min(self.rows.len() - 1);
        } else {
            self.selected = 0;
        }

        // Update agent map and compute active indices for 'a' cycling
        self.agent_map = agent_map.clone();
        self.agent_active_indices = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.active_agent_count > 0 && r.back_ref.is_none())
            .map(|(i, _)| i)
            .collect();

        // Always compute DAG layout so it's ready when user switches modes
        let mut dag = DagLayout::compute(&graph, &critical_ids, &agent_map);
        super::graph_layout::center_layers(&mut dag);
        super::graph_layout::reroute_edges(&mut dag, &graph);

        // Preserve DAG selection by task ID
        let prev_dag_id = self
            .graph_layout
            .as_ref()
            .and_then(|l| l.nodes.get(self.dag_selected).map(|n| n.task_id.clone()));
        if let Some(ref id) = prev_dag_id
            && let Some(&idx) = dag.id_to_idx.get(id)
        {
            self.dag_selected = idx;
        }
        if !dag.nodes.is_empty() {
            self.dag_selected = self.dag_selected.min(dag.nodes.len() - 1);
        } else {
            self.dag_selected = 0;
        }

        self.graph_layout = Some(dag);
    }

    pub fn scroll_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn scroll_down(&mut self) {
        if !self.rows.is_empty() {
            self.selected = (self.selected + 1).min(self.rows.len() - 1);
        }
    }

    /// Collapse the subtree of the selected node (Left arrow)
    pub fn collapse(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            if row.back_ref.is_some() {
                return; // Can't collapse back-references
            }
            self.collapsed_ids.insert(row.task_id.clone());
        }
    }

    /// Expand the subtree of the selected node (Right arrow)
    pub fn expand(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            self.collapsed_ids.remove(&row.task_id);
        }
    }

    /// Toggle detail overlay for the selected task
    pub fn toggle_detail(&mut self, workgraph_dir: &std::path::Path) {
        if self.show_detail {
            self.show_detail = false;
            self.detail_task = None;
            self.detail_scroll = 0;
            return;
        }
        if let Some(row) = self.rows.get(self.selected) {
            let graph_path = workgraph_dir.join("graph.jsonl");
            if let Ok(graph) = load_graph(&graph_path)
                && let Some(task) = graph.get_task(&row.task_id)
            {
                self.detail_task = Some(task.clone());
                self.show_detail = true;
                self.detail_scroll = 0;
            }
        }
    }

    pub fn detail_scroll_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(1);
    }

    pub fn detail_scroll_down(&mut self) {
        self.detail_scroll += 1;
    }

    /// Cycle selection to the next task with active agents ('a' key)
    pub fn cycle_to_next_agent_task(&mut self) {
        if self.agent_active_indices.is_empty() {
            return;
        }
        // Find the next index after the current selection
        let next = self
            .agent_active_indices
            .iter()
            .find(|&&idx| idx > self.selected)
            .or_else(|| self.agent_active_indices.first());
        if let Some(&idx) = next {
            self.selected = idx;
        }
    }

    /// Get the first active agent ID for the currently selected task
    pub fn selected_task_first_agent(&self) -> Option<String> {
        match self.view_mode {
            GraphViewMode::Tree => {
                let row = self.rows.get(self.selected)?;
                if row.active_agent_count > 0 {
                    row.active_agent_ids.first().cloned()
                } else {
                    None
                }
            }
            GraphViewMode::Dag => {
                let layout = self.graph_layout.as_ref()?;
                let node = layout.nodes.get(self.dag_selected)?;
                if node.active_agent_count > 0 {
                    node.active_agent_ids.first().cloned()
                } else {
                    None
                }
            }
        }
    }

    /// Toggle between tree and graph view modes
    pub fn toggle_view_mode(&mut self) {
        self.view_mode = match self.view_mode {
            GraphViewMode::Tree => GraphViewMode::Dag,
            GraphViewMode::Dag => GraphViewMode::Tree,
        };
    }

    /// DAG mode: move selection to the next node
    pub fn dag_select_next(&mut self) {
        if let Some(ref layout) = self.graph_layout
            && !layout.nodes.is_empty()
        {
            self.dag_selected = (self.dag_selected + 1).min(layout.nodes.len() - 1);
        }
    }

    /// DAG mode: move selection to the previous node
    pub fn dag_select_prev(&mut self) {
        self.dag_selected = self.dag_selected.saturating_sub(1);
    }

    /// DAG mode: scroll view left
    pub fn dag_scroll_left(&mut self) {
        self.dag_scroll_x = self.dag_scroll_x.saturating_sub(4);
    }

    /// DAG mode: scroll view right
    pub fn dag_scroll_right(&mut self) {
        self.dag_scroll_x += 4;
    }

    /// DAG mode: get the selected task ID
    pub fn dag_selected_task_id(&self) -> Option<&str> {
        self.graph_layout
            .as_ref()
            .and_then(|l| l.nodes.get(self.dag_selected))
            .map(|n| n.task_id.as_str())
    }

    /// DAG mode: toggle detail overlay for the selected task
    pub fn dag_toggle_detail(&mut self, workgraph_dir: &std::path::Path) {
        if self.show_detail {
            self.show_detail = false;
            self.detail_task = None;
            self.detail_scroll = 0;
            return;
        }
        if let Some(task_id) = self
            .dag_selected_task_id()
            .map(std::string::ToString::to_string)
        {
            let graph_path = workgraph_dir.join("graph.jsonl");
            if let Ok(graph) = load_graph(&graph_path)
                && let Some(task) = graph.get_task(&task_id)
            {
                self.detail_task = Some(task.clone());
                self.show_detail = true;
                self.detail_scroll = 0;
            }
        }
    }

    /// DAG mode: ensure the selected node is visible in the viewport
    pub fn dag_ensure_visible(&mut self, viewport_width: u16, viewport_height: u16) {
        if let Some(ref layout) = self.graph_layout
            && let Some(node) = layout.nodes.get(self.dag_selected)
        {
            let vw = viewport_width as usize;
            let vh = viewport_height as usize;

            // Horizontal: ensure node is visible
            if node.x < self.dag_scroll_x {
                self.dag_scroll_x = node.x.saturating_sub(2);
            } else if node.x + node.w > self.dag_scroll_x + vw {
                self.dag_scroll_x = (node.x + node.w).saturating_sub(vw) + 2;
            }

            // Vertical: ensure node is visible
            if node.y < self.dag_scroll_y {
                self.dag_scroll_y = node.y.saturating_sub(1);
            } else if node.y + node.h > self.dag_scroll_y + vh {
                self.dag_scroll_y = (node.y + node.h).saturating_sub(vh) + 1;
            }
        }
    }
}

/// Build the flattened tree representation of the task dependency graph.
///
/// Strategy: DFS from root nodes, then render tasks as an indented tree.
/// Root nodes are tasks with no `after`. Each task is shown indented
/// under its last blocker. Tasks with multiple blockers get a back-reference
/// marker under earlier blockers.
fn build_graph_tree(
    graph: &WorkGraph,
    collapsed: &HashSet<String>,
    critical_ids: &HashSet<String>,
    agent_map: &HashMap<String, TaskAgentInfo>,
) -> Vec<GraphRow> {
    // Collect all tasks and build adjacency
    let tasks: HashMap<String, &Task> = graph.tasks().map(|t| (t.id.clone(), t)).collect();
    // children[parent_id] = list of task IDs that are after parent_id
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots: Vec<String> = Vec::new();

    for task in tasks.values() {
        if task.after.is_empty() {
            roots.push(task.id.clone());
        }
        for blocker_id in &task.after {
            children
                .entry(blocker_id.clone())
                .or_default()
                .push(task.id.clone());
        }
    }

    // Sort roots by status priority then title
    roots.sort_by(|a, b| {
        let ta = tasks.get(a);
        let tb = tasks.get(b);
        match (ta, tb) {
            (Some(a), Some(b)) => sort_key_for_status(&a.status)
                .cmp(&sort_key_for_status(&b.status))
                .then(a.title.cmp(&b.title)),
            _ => a.cmp(b),
        }
    });

    // Sort children lists similarly
    for kids in children.values_mut() {
        kids.sort_by(|a, b| {
            let ta = tasks.get(a);
            let tb = tasks.get(b);
            match (ta, tb) {
                (Some(a), Some(b)) => sort_key_for_status(&a.status)
                    .cmp(&sort_key_for_status(&b.status))
                    .then(a.title.cmp(&b.title)),
                _ => a.cmp(b),
            }
        });
    }

    // DFS to flatten tree, tracking which tasks we've placed canonically
    let mut rows = Vec::new();
    let mut placed: HashSet<String> = HashSet::new();

    for root_id in &roots {
        flatten_subtree(
            root_id,
            &tasks,
            &children,
            collapsed,
            critical_ids,
            agent_map,
            &mut placed,
            &mut rows,
            0,
        );
    }

    // Handle orphan tasks that weren't reached (e.g., cycles or missing blockers)
    let mut orphans: Vec<String> = tasks
        .keys()
        .filter(|id| !placed.contains(*id))
        .cloned()
        .collect();
    orphans.sort();
    for id in orphans {
        if let Some(task) = tasks.get(&id) {
            placed.insert(id.clone());
            let (agent_count, agent_ids) = agent_map
                .get(&task.id)
                .map(|info| (info.count, info.agent_ids.clone()))
                .unwrap_or((0, Vec::new()));
            rows.push(GraphRow {
                task_id: task.id.clone(),
                title: task.title.clone(),
                status: task.status,
                assigned: task.assigned.clone(),
                depth: 0,
                collapsed: collapsed.contains(&task.id),
                critical: critical_ids.contains(&task.id),
                back_ref: None,
                active_agent_count: agent_count,
                active_agent_ids: agent_ids,
            });
        }
    }

    rows
}

#[allow(clippy::too_many_arguments)]
fn flatten_subtree(
    task_id: &str,
    tasks: &HashMap<String, &Task>,
    children: &HashMap<String, Vec<String>>,
    collapsed: &HashSet<String>,
    critical_ids: &HashSet<String>,
    agent_map: &HashMap<String, TaskAgentInfo>,
    placed: &mut HashSet<String>,
    rows: &mut Vec<GraphRow>,
    depth: usize,
) {
    let Some(task) = tasks.get(task_id) else {
        return;
    };

    let is_back_ref = placed.contains(task_id);
    let (agent_count, agent_ids) = agent_map
        .get(task_id)
        .map(|info| (info.count, info.agent_ids.clone()))
        .unwrap_or((0, Vec::new()));

    if is_back_ref {
        // Show a back-reference row
        rows.push(GraphRow {
            task_id: task.id.clone(),
            title: task.title.clone(),
            status: task.status,
            assigned: task.assigned.clone(),
            depth,
            collapsed: false,
            critical: critical_ids.contains(&task.id),
            back_ref: Some(task.id.clone()),
            active_agent_count: agent_count,
            active_agent_ids: agent_ids,
        });
        return;
    }

    placed.insert(task_id.to_string());
    let is_collapsed = collapsed.contains(task_id);
    rows.push(GraphRow {
        task_id: task.id.clone(),
        title: task.title.clone(),
        status: task.status,
        assigned: task.assigned.clone(),
        depth,
        collapsed: is_collapsed,
        critical: critical_ids.contains(&task.id),
        back_ref: None,
        active_agent_count: agent_count,
        active_agent_ids: agent_ids,
    });

    if is_collapsed {
        return;
    }

    if let Some(kids) = children.get(task_id) {
        for kid_id in kids {
            flatten_subtree(
                kid_id,
                tasks,
                children,
                collapsed,
                critical_ids,
                agent_map,
                placed,
                rows,
                depth + 1,
            );
        }
    }
}

fn sort_key_for_status(status: &Status) -> u8 {
    match status {
        Status::InProgress => 0,
        Status::Open => 1,
        Status::Failed => 2,
        Status::Blocked => 3,
        Status::Done => 4,
        Status::Abandoned => 5,
    }
}

/// Compute the critical path: the longest chain of incomplete tasks.
/// Returns the set of task IDs on the critical path.
fn compute_critical_path(graph: &WorkGraph) -> HashSet<String> {
    let tasks: HashMap<String, &Task> = graph.tasks().map(|t| (t.id.clone(), t)).collect();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots: Vec<String> = Vec::new();

    for task in tasks.values() {
        if task.after.is_empty() {
            roots.push(task.id.clone());
        }
        for blocker_id in &task.after {
            children
                .entry(blocker_id.clone())
                .or_default()
                .push(task.id.clone());
        }
    }

    // For each task, compute the longest chain of incomplete tasks reachable from it
    let mut memo: HashMap<String, usize> = HashMap::new();
    let mut path_next: HashMap<String, String> = HashMap::new();

    fn longest_chain(
        task_id: &str,
        tasks: &HashMap<String, &Task>,
        children: &HashMap<String, Vec<String>>,
        memo: &mut HashMap<String, usize>,
        path_next: &mut HashMap<String, String>,
        visiting: &mut HashSet<String>,
    ) -> usize {
        if let Some(&cached) = memo.get(task_id) {
            return cached;
        }
        if visiting.contains(task_id) {
            return 0; // cycle guard
        }
        visiting.insert(task_id.to_string());

        let task = match tasks.get(task_id) {
            Some(t) => t,
            None => return 0,
        };

        // Only count incomplete tasks
        let self_weight = match task.status {
            Status::Done | Status::Abandoned => 0,
            _ => 1,
        };

        let mut best_child_len = 0;
        let mut best_child_id = None;

        if let Some(kids) = children.get(task_id) {
            for kid_id in kids {
                let child_len = longest_chain(kid_id, tasks, children, memo, path_next, visiting);
                if child_len > best_child_len {
                    best_child_len = child_len;
                    best_child_id = Some(kid_id.clone());
                }
            }
        }

        let total = self_weight + best_child_len;
        memo.insert(task_id.to_string(), total);
        if let Some(next) = best_child_id {
            path_next.insert(task_id.to_string(), next);
        }

        visiting.remove(task_id);
        total
    }

    // Find the root with the longest chain
    let mut best_root = None;
    let mut best_len = 0;

    for root_id in &roots {
        let len = longest_chain(
            root_id,
            &tasks,
            &children,
            &mut memo,
            &mut path_next,
            &mut HashSet::new(),
        );
        if len > best_len {
            best_len = len;
            best_root = Some(root_id.clone());
        }
    }

    // Walk the critical path
    let mut critical = HashSet::new();
    if let Some(start) = best_root {
        let mut current = start;
        loop {
            if let Some(task) = tasks.get(&current)
                && !matches!(task.status, Status::Done | Status::Abandoned)
            {
                critical.insert(current.clone());
            }
            match path_next.get(&current) {
                Some(next) => current = next.clone(),
                None => break,
            }
        }
    }

    critical
}

/// Summary counts for the status bar
#[derive(Debug, Default)]
pub struct TaskCounts {
    pub done: usize,
    pub in_progress: usize,
    pub ready: usize,
    pub blocked: usize,
    pub failed: usize,
    pub total: usize,
}

/// Snapshot of a task's key fields for change detection
#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskSnapshot {
    status: String,
    assigned: Option<String>,
}

/// Snapshot of an agent's key fields for change detection
#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentSnapshot {
    status: String,
    task_id: String,
}

/// Top-level application state for the TUI
pub struct App {
    /// Current view (Dashboard, LogView, or GraphExplorer)
    pub view: View,

    /// Log viewer state (populated when viewing an agent's log)
    pub log_viewer: Option<LogViewer>,

    /// Graph explorer state (populated when viewing the dependency graph)
    pub graph_explorer: Option<GraphExplorer>,

    /// Which panel is selected
    pub selected_panel: Panel,

    /// Selected task index in the task list
    pub task_selected: usize,

    /// Selected agent index in the agent list
    pub agent_selected: usize,

    /// Path to the .workgraph directory
    pub workgraph_dir: PathBuf,

    /// Whether the app should quit
    pub should_quit: bool,

    /// Loaded and sorted task entries
    pub tasks: Vec<TaskEntry>,

    /// Task count summary
    pub task_counts: TaskCounts,

    /// Loaded agents (refreshed periodically)
    pub agents: Vec<AgentInfo>,

    /// Agent counts: (alive, dead, total)
    pub agent_counts: (usize, usize, usize),

    /// Task IDs that recently changed (for flash highlight)
    pub highlighted_tasks: HashMap<String, Instant>,

    /// Agent IDs that recently changed (for flash highlight)
    pub highlighted_agents: HashMap<String, Instant>,

    /// Previous task snapshots for diffing
    prev_task_snapshots: HashMap<String, TaskSnapshot>,

    /// Previous agent snapshots for diffing
    prev_agent_snapshots: HashMap<String, AgentSnapshot>,

    /// Previously known task IDs (for detecting new tasks)
    prev_task_ids: HashSet<String>,

    /// Previously known agent IDs (for detecting new agents)
    prev_agent_ids: HashSet<String>,

    /// Last time we refreshed data
    pub last_refresh: Instant,

    /// Display string for last refresh time (HH:MM:SS)
    pub last_refresh_display: String,

    /// Configurable poll interval
    pub poll_interval: Duration,

    /// Whether the help overlay is visible
    pub show_help: bool,

    /// Whether this is the first data load (skip highlighting on first load)
    first_load: bool,
}

impl App {
    pub fn new(workgraph_dir: PathBuf, poll_interval: Duration) -> Self {
        let mut app = Self {
            view: View::Dashboard,
            log_viewer: None,
            graph_explorer: None,
            selected_panel: Panel::Tasks,
            task_selected: 0,
            agent_selected: 0,
            workgraph_dir,
            should_quit: false,
            tasks: Vec::new(),
            task_counts: TaskCounts::default(),
            agents: Vec::new(),
            agent_counts: (0, 0, 0),
            highlighted_tasks: HashMap::new(),
            highlighted_agents: HashMap::new(),
            prev_task_snapshots: HashMap::new(),
            prev_agent_snapshots: HashMap::new(),
            prev_task_ids: HashSet::new(),
            prev_agent_ids: HashSet::new(),
            last_refresh: Instant::now(),
            last_refresh_display: String::from("--:--:--"),
            poll_interval,
            show_help: false,
            first_load: true,
        };
        app.refresh_all();
        app
    }

    /// Check if enough time has elapsed since the last poll; if so, refresh
    pub fn maybe_refresh(&mut self) {
        if self.last_refresh.elapsed() >= self.poll_interval {
            self.refresh_all();
        }
    }

    /// Refresh both tasks and agents, diff, update highlights, preserve selection
    pub fn refresh_all(&mut self) {
        // Remember current selection keys to restore after re-sort
        let prev_task_id = self.tasks.get(self.task_selected).map(|t| t.id.clone());
        let prev_agent_id = self.agents.get(self.agent_selected).map(|a| a.id.clone());

        self.load_tasks();
        self.load_agents();

        // Restore selection position by ID
        if let Some(ref id) = prev_task_id
            && let Some(pos) = self.tasks.iter().position(|t| t.id == *id)
        {
            self.task_selected = pos;
        }
        if let Some(ref id) = prev_agent_id
            && let Some(pos) = self.agents.iter().position(|a| a.id == *id)
        {
            self.agent_selected = pos;
        }

        // Clamp selections
        if !self.tasks.is_empty() {
            self.task_selected = self.task_selected.min(self.tasks.len() - 1);
        } else {
            self.task_selected = 0;
        }
        if !self.agents.is_empty() {
            self.agent_selected = self.agent_selected.min(self.agents.len() - 1);
        } else {
            self.agent_selected = 0;
        }

        // Expire old highlights
        let now = Instant::now();
        self.highlighted_tasks
            .retain(|_, t| now.duration_since(*t) < HIGHLIGHT_DURATION);
        self.highlighted_agents
            .retain(|_, t| now.duration_since(*t) < HIGHLIGHT_DURATION);

        // Also refresh graph explorer if it's open (keeps agent overlay live)
        if self.graph_explorer.is_some() {
            self.refresh_graph_explorer();
        }

        // Update refresh metadata
        self.last_refresh = Instant::now();
        self.last_refresh_display = chrono::Local::now().format("%H:%M:%S").to_string();
        self.first_load = false;
    }

    /// Load tasks from graph.jsonl, sort, compute counts, diff for highlights
    fn load_tasks(&mut self) {
        let graph_path = self.workgraph_dir.join("graph.jsonl");
        let graph = match load_graph(&graph_path) {
            Ok(g) => g,
            Err(_) => {
                self.tasks.clear();
                self.task_counts = TaskCounts::default();
                return;
            }
        };

        let mut entries: Vec<TaskEntry> = graph
            .tasks()
            .map(|t: &Task| TaskEntry {
                id: t.id.clone(),
                title: t.title.clone(),
                status: t.status,
                assigned: t.assigned.clone(),
            })
            .collect();

        entries.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()).then(a.title.cmp(&b.title)));

        // Compute counts
        let mut counts = TaskCounts {
            total: entries.len(),
            ..TaskCounts::default()
        };
        for entry in &entries {
            match entry.status {
                Status::Done => counts.done += 1,
                Status::InProgress => counts.in_progress += 1,
                Status::Open => counts.ready += 1,
                Status::Blocked => counts.blocked += 1,
                Status::Failed => counts.failed += 1,
                Status::Abandoned => counts.done += 1,
            }
        }

        // Diff for highlights (skip on first load to avoid highlighting everything)
        if !self.first_load {
            // Detect new tasks
            for t in &entries {
                if !self.prev_task_ids.contains(&t.id) {
                    self.highlighted_tasks.insert(t.id.clone(), Instant::now());
                }
            }

            // Detect changed tasks
            for t in &entries {
                let snap = TaskSnapshot {
                    status: format!("{:?}", t.status),
                    assigned: t.assigned.clone(),
                };
                if let Some(prev) = self.prev_task_snapshots.get(&t.id)
                    && *prev != snap
                {
                    self.highlighted_tasks.insert(t.id.clone(), Instant::now());
                }
            }
        }

        // Update snapshots for next diff
        self.prev_task_ids = entries.iter().map(|t| t.id.clone()).collect();
        self.prev_task_snapshots = entries
            .iter()
            .map(|t| {
                (
                    t.id.clone(),
                    TaskSnapshot {
                        status: format!("{:?}", t.status),
                        assigned: t.assigned.clone(),
                    },
                )
            })
            .collect();

        self.tasks = entries;
        self.task_counts = counts;
    }

    /// Load agents from registry.json, sort, diff for highlights
    fn load_agents(&mut self) {
        let registry = match AgentRegistry::load(&self.workgraph_dir) {
            Ok(r) => r,
            Err(_) => {
                self.agents.clear();
                self.agent_counts = (0, 0, 0);
                return;
            }
        };

        let mut agents: Vec<AgentInfo> = registry
            .list_agents()
            .into_iter()
            .map(AgentInfo::from_entry)
            .collect();

        agents.sort_by(|a, b| {
            let order = |s: &AgentStatus| -> u8 {
                match s {
                    AgentStatus::Working => 0,
                    AgentStatus::Starting => 1,
                    AgentStatus::Idle => 2,
                    AgentStatus::Stopping => 3,
                    AgentStatus::Dead => 4,
                    AgentStatus::Failed => 5,
                    AgentStatus::Done => 6,
                }
            };
            order(&a.status)
                .cmp(&order(&b.status))
                .then_with(|| b.id.cmp(&a.id)) // newest first within same status group
        });

        let alive = agents.iter().filter(|a| a.is_alive()).count();
        let dead = agents.iter().filter(|a| a.is_dead()).count();
        let total = agents.len();

        // Diff for highlights (skip on first load)
        if !self.first_load {
            for a in &agents {
                if !self.prev_agent_ids.contains(&a.id) {
                    self.highlighted_agents.insert(a.id.clone(), Instant::now());
                }
            }

            for a in &agents {
                let snap = AgentSnapshot {
                    status: format!("{:?}", a.status),
                    task_id: a.task_id.clone(),
                };
                if let Some(prev) = self.prev_agent_snapshots.get(&a.id)
                    && *prev != snap
                {
                    self.highlighted_agents.insert(a.id.clone(), Instant::now());
                }
            }
        }

        // Update snapshots for next diff
        self.prev_agent_ids = agents.iter().map(|a| a.id.clone()).collect();
        self.prev_agent_snapshots = agents
            .iter()
            .map(|a| {
                (
                    a.id.clone(),
                    AgentSnapshot {
                        status: format!("{:?}", a.status),
                        task_id: a.task_id.clone(),
                    },
                )
            })
            .collect();

        self.agent_counts = (alive, dead, total);
        self.agents = agents;
    }

    /// Check if a task is currently highlighted (recently changed)
    pub fn is_task_highlighted(&self, id: &str) -> bool {
        match self.highlighted_tasks.get(id) {
            Some(t) => Instant::now().duration_since(*t) < HIGHLIGHT_DURATION,
            None => false,
        }
    }

    /// Check if an agent is currently highlighted (recently changed)
    pub fn is_agent_highlighted(&self, id: &str) -> bool {
        match self.highlighted_agents.get(id) {
            Some(t) => Instant::now().duration_since(*t) < HIGHLIGHT_DURATION,
            None => false,
        }
    }

    /// Toggle between panels
    pub fn toggle_panel(&mut self) {
        self.selected_panel = match self.selected_panel {
            Panel::Tasks => Panel::Agents,
            Panel::Agents => Panel::Tasks,
        };
    }

    /// Scroll the active panel up
    pub fn scroll_up(&mut self) {
        match self.selected_panel {
            Panel::Tasks => {
                self.task_selected = self.task_selected.saturating_sub(1);
            }
            Panel::Agents => {
                self.agent_selected = self.agent_selected.saturating_sub(1);
            }
        }
    }

    /// Scroll the active panel down
    pub fn scroll_down(&mut self) {
        match self.selected_panel {
            Panel::Tasks => {
                if !self.tasks.is_empty() {
                    self.task_selected = (self.task_selected + 1).min(self.tasks.len() - 1);
                }
            }
            Panel::Agents => {
                if !self.agents.is_empty() {
                    self.agent_selected = (self.agent_selected + 1).min(self.agents.len() - 1);
                }
            }
        }
    }

    /// Drill into the selected item (Enter key from dashboard)
    pub fn drill_in(&mut self) {
        match self.selected_panel {
            Panel::Agents => self.open_log_viewer(),
            Panel::Tasks => {
                // Open graph explorer focused on the selected task
                if let Some(task) = self.tasks.get(self.task_selected) {
                    let task_id = task.id.clone();
                    let mut explorer = GraphExplorer::new(&self.workgraph_dir);
                    // Try to select the task in the graph
                    if let Some(pos) = explorer
                        .rows
                        .iter()
                        .position(|r| r.task_id == task_id && r.back_ref.is_none())
                    {
                        explorer.selected = pos;
                    }
                    self.graph_explorer = Some(explorer);
                    self.view = View::GraphExplorer;
                }
            }
        }
    }

    /// Open the log viewer for the currently selected agent
    pub fn open_log_viewer(&mut self) {
        if self.selected_panel != Panel::Agents || self.agents.is_empty() {
            return;
        }
        let agent = self.agents[self.agent_selected].clone();
        self.log_viewer = Some(LogViewer::new(agent));
        self.view = View::LogView;
    }

    /// Open the log viewer for a specific agent by ID (used from graph explorer)
    pub fn open_log_viewer_for_agent(&mut self, agent_id: &str) {
        if let Some(agent) = self.agents.iter().find(|a| a.id == agent_id) {
            let agent = agent.clone();
            self.log_viewer = Some(LogViewer::new(agent));
            self.view = View::LogView;
            self.graph_explorer = None;
        }
    }

    /// Close the log viewer and return to the dashboard
    pub fn close_log_viewer(&mut self) {
        self.log_viewer = None;
        self.view = View::Dashboard;
    }

    /// Open the graph explorer view
    pub fn open_graph_explorer(&mut self) {
        let explorer = GraphExplorer::new(&self.workgraph_dir);
        self.graph_explorer = Some(explorer);
        self.view = View::GraphExplorer;
    }

    /// Close the graph explorer and return to the dashboard
    pub fn close_graph_explorer(&mut self) {
        self.graph_explorer = None;
        self.view = View::Dashboard;
    }

    /// Refresh the graph explorer data
    pub fn refresh_graph_explorer(&mut self) {
        if let Some(ref mut explorer) = self.graph_explorer {
            explorer.rebuild(&self.workgraph_dir);
        }
    }

    /// Poll the log viewer for new content (called from event loop)
    pub fn poll_log_viewer(&mut self) {
        if let Some(ref mut viewer) = self.log_viewer {
            viewer.poll_file();
        }
    }

    /// Return the current view label
    pub fn view_label(&self) -> &'static str {
        match self.view {
            View::Dashboard => "Dashboard",
            View::LogView => "Log Viewer",
            View::GraphExplorer => "Graph Explorer",
        }
    }

    /// Return key hints for the current view
    pub fn key_hints(&self) -> &'static str {
        match self.view {
            View::Dashboard => "q=quit ?=help Tab=switch j/k=nav Enter=drill-in g=graph r=refresh",
            View::LogView => "q=quit ?=help Esc=back j/k=scroll PgUp/PgDn g=top G=bottom",
            View::GraphExplorer => {
                "q=quit ?=help Esc=back d=toggle view j/k=nav Enter=details r=refresh"
            }
        }
    }

    /// Check whether the service daemon is running
    pub fn is_service_running(&self) -> bool {
        if let Ok(Some(state)) = crate::commands::service::ServiceState::load(&self.workgraph_dir) {
            return is_process_alive(state.pid);
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::super::graph_layout::DagLayout;
    use super::*;
    use ratatui::style::Color;
    use workgraph::AgentStatus;
    use workgraph::graph::Status;

    // ── Helpers ──────────────────────────────────────────────────────

    /// Build a minimal GraphExplorer without filesystem access.
    fn make_graph_explorer(rows: Vec<GraphRow>) -> GraphExplorer {
        GraphExplorer {
            rows,
            selected: 0,
            collapsed_ids: HashSet::new(),
            show_detail: false,
            detail_scroll: 0,
            detail_task: None,
            agent_map: HashMap::new(),
            agent_active_indices: Vec::new(),
            view_mode: GraphViewMode::Tree,
            graph_layout: None,
            dag_selected: 0,
            dag_scroll_x: 0,
            dag_scroll_y: 0,
        }
    }

    fn make_row(id: &str, status: Status) -> GraphRow {
        GraphRow {
            task_id: id.to_string(),
            title: id.to_string(),
            status,
            assigned: None,
            depth: 0,
            collapsed: false,
            critical: false,
            back_ref: None,
            active_agent_count: 0,
            active_agent_ids: Vec::new(),
        }
    }

    /// Build a LogViewer with pre-loaded lines (no file I/O).
    fn make_log_viewer(lines: Vec<&str>) -> LogViewer {
        LogViewer {
            agent: AgentInfo {
                id: "test-agent".to_string(),
                task_id: "test-task".to_string(),
                executor: "test".to_string(),
                pid: 0,
                uptime: "0s".to_string(),
                status: AgentStatus::Working,
                output_file: String::new(),
            },
            lines: lines.into_iter().map(|s| s.to_string()).collect(),
            scroll_offset: 0,
            auto_scroll: true,
            file_pos: 0,
            last_poll: Instant::now(),
        }
    }

    /// Build a minimal App without filesystem access.
    fn make_app() -> App {
        App {
            view: View::Dashboard,
            log_viewer: None,
            graph_explorer: None,
            selected_panel: Panel::Tasks,
            task_selected: 0,
            agent_selected: 0,
            workgraph_dir: PathBuf::from("/tmp/nonexistent-wg-test"),
            should_quit: false,
            tasks: Vec::new(),
            task_counts: TaskCounts::default(),
            agents: Vec::new(),
            agent_counts: (0, 0, 0),
            highlighted_tasks: HashMap::new(),
            highlighted_agents: HashMap::new(),
            prev_task_snapshots: HashMap::new(),
            prev_agent_snapshots: HashMap::new(),
            prev_task_ids: HashSet::new(),
            prev_agent_ids: HashSet::new(),
            last_refresh: Instant::now(),
            last_refresh_display: String::from("--:--:--"),
            poll_interval: Duration::from_secs(999),
            show_help: false,
            first_load: false,
        }
    }

    fn sample_tasks() -> Vec<TaskEntry> {
        vec![
            TaskEntry {
                id: "a".into(),
                title: "Alpha".into(),
                status: Status::InProgress,
                assigned: None,
            },
            TaskEntry {
                id: "b".into(),
                title: "Beta".into(),
                status: Status::Open,
                assigned: None,
            },
            TaskEntry {
                id: "c".into(),
                title: "Gamma".into(),
                status: Status::Done,
                assigned: None,
            },
        ]
    }

    fn sample_agents() -> Vec<AgentInfo> {
        vec![
            AgentInfo {
                id: "agent-1".into(),
                task_id: "a".into(),
                executor: "claude".into(),
                pid: 1,
                uptime: "1m".into(),
                status: AgentStatus::Working,
                output_file: String::new(),
            },
            AgentInfo {
                id: "agent-2".into(),
                task_id: "b".into(),
                executor: "claude".into(),
                pid: 2,
                uptime: "2m".into(),
                status: AgentStatus::Done,
                output_file: String::new(),
            },
        ]
    }

    // ── TaskEntry::sort_key ─────────────────────────────────────────

    #[test]
    fn task_entry_sort_key_ordering() {
        let statuses = [
            (Status::InProgress, 0u8),
            (Status::Open, 1),
            (Status::Failed, 2),
            (Status::Blocked, 3),
            (Status::Done, 4),
            (Status::Abandoned, 5),
        ];
        for (status, expected) in &statuses {
            let entry = TaskEntry {
                id: "x".into(),
                title: "x".into(),
                status: status.clone(),
                assigned: None,
            };
            assert_eq!(
                entry.sort_key(),
                *expected,
                "sort_key mismatch for {:?}",
                status
            );
        }
    }

    #[test]
    fn task_entry_sort_key_is_monotonic() {
        let ordered = [
            Status::InProgress,
            Status::Open,
            Status::Failed,
            Status::Blocked,
            Status::Done,
            Status::Abandoned,
        ];
        for w in ordered.windows(2) {
            let a = TaskEntry {
                id: "x".into(),
                title: "x".into(),
                status: w[0].clone(),
                assigned: None,
            };
            let b = TaskEntry {
                id: "x".into(),
                title: "x".into(),
                status: w[1].clone(),
                assigned: None,
            };
            assert!(
                a.sort_key() < b.sort_key(),
                "{:?} should sort before {:?}",
                w[0],
                w[1]
            );
        }
    }

    // ── Pure functions from tui::mod ────────────────────────────────

    #[test]
    fn agent_status_color_all_variants() {
        use super::super::agent_status_color;
        assert_eq!(agent_status_color(&AgentStatus::Working), Color::Green);
        assert_eq!(agent_status_color(&AgentStatus::Starting), Color::Yellow);
        assert_eq!(agent_status_color(&AgentStatus::Idle), Color::Cyan);
        assert_eq!(agent_status_color(&AgentStatus::Stopping), Color::Yellow);
        assert_eq!(agent_status_color(&AgentStatus::Dead), Color::Red);
        assert_eq!(agent_status_color(&AgentStatus::Failed), Color::Red);
        assert_eq!(agent_status_color(&AgentStatus::Done), Color::DarkGray);
    }

    #[test]
    fn status_color_all_variants() {
        use super::super::status_color;
        assert_eq!(status_color(&Status::Done), Color::Green);
        assert_eq!(status_color(&Status::InProgress), Color::Yellow);
        assert_eq!(status_color(&Status::Open), Color::White);
        assert_eq!(status_color(&Status::Failed), Color::Red);
        assert_eq!(status_color(&Status::Blocked), Color::DarkGray);
        assert_eq!(status_color(&Status::Abandoned), Color::DarkGray);
    }

    #[test]
    fn status_indicator_all_variants() {
        use super::super::status_indicator;
        assert_eq!(status_indicator(&Status::Done), "[x]");
        assert_eq!(status_indicator(&Status::InProgress), "[~]");
        assert_eq!(status_indicator(&Status::Open), "[ ]");
        assert_eq!(status_indicator(&Status::Failed), "[!]");
        assert_eq!(status_indicator(&Status::Blocked), "[B]");
        assert_eq!(status_indicator(&Status::Abandoned), "[-]");
    }

    #[test]
    fn agent_status_label_all_variants() {
        use super::super::agent_status_label;
        assert_eq!(agent_status_label(&AgentStatus::Working), "WORKING");
        assert_eq!(agent_status_label(&AgentStatus::Starting), "STARTING");
        assert_eq!(agent_status_label(&AgentStatus::Idle), "IDLE");
        assert_eq!(agent_status_label(&AgentStatus::Stopping), "STOPPING");
        assert_eq!(agent_status_label(&AgentStatus::Dead), "DEAD");
        assert_eq!(agent_status_label(&AgentStatus::Failed), "FAILED");
        assert_eq!(agent_status_label(&AgentStatus::Done), "DONE");
    }

    // ── GraphExplorer state mutations ───────────────────────────────

    #[test]
    fn graph_explorer_scroll_up_at_zero_stays_zero() {
        let mut ex = make_graph_explorer(vec![make_row("a", Status::Open)]);
        ex.scroll_up();
        assert_eq!(ex.selected, 0);
    }

    #[test]
    fn graph_explorer_scroll_down_advances() {
        let rows = vec![
            make_row("a", Status::Open),
            make_row("b", Status::Open),
            make_row("c", Status::Open),
        ];
        let mut ex = make_graph_explorer(rows);
        ex.scroll_down();
        assert_eq!(ex.selected, 1);
        ex.scroll_down();
        assert_eq!(ex.selected, 2);
    }

    #[test]
    fn graph_explorer_scroll_down_clamps_at_end() {
        let rows = vec![make_row("a", Status::Open), make_row("b", Status::Open)];
        let mut ex = make_graph_explorer(rows);
        ex.scroll_down();
        ex.scroll_down();
        ex.scroll_down();
        assert_eq!(ex.selected, 1);
    }

    #[test]
    fn graph_explorer_scroll_down_empty_noop() {
        let mut ex = make_graph_explorer(vec![]);
        ex.scroll_down();
        assert_eq!(ex.selected, 0);
    }

    #[test]
    fn graph_explorer_scroll_up_decrements() {
        let rows = vec![make_row("a", Status::Open), make_row("b", Status::Open)];
        let mut ex = make_graph_explorer(rows);
        ex.selected = 1;
        ex.scroll_up();
        assert_eq!(ex.selected, 0);
    }

    #[test]
    fn graph_explorer_collapse_adds_to_collapsed_ids() {
        let rows = vec![make_row("task-1", Status::Open)];
        let mut ex = make_graph_explorer(rows);
        ex.collapse();
        assert!(ex.collapsed_ids.contains("task-1"));
    }

    #[test]
    fn graph_explorer_expand_removes_from_collapsed_ids() {
        let rows = vec![make_row("task-1", Status::Open)];
        let mut ex = make_graph_explorer(rows);
        ex.collapsed_ids.insert("task-1".to_string());
        ex.expand();
        assert!(!ex.collapsed_ids.contains("task-1"));
    }

    #[test]
    fn graph_explorer_collapse_back_ref_is_noop() {
        let mut row = make_row("task-1", Status::Open);
        row.back_ref = Some("task-1".to_string());
        let mut ex = make_graph_explorer(vec![row]);
        ex.collapse();
        assert!(!ex.collapsed_ids.contains("task-1"));
    }

    #[test]
    fn graph_explorer_detail_scroll() {
        let mut ex = make_graph_explorer(vec![make_row("a", Status::Open)]);
        ex.show_detail = true;
        ex.detail_scroll = 5;
        ex.detail_scroll_up();
        assert_eq!(ex.detail_scroll, 4);
        ex.detail_scroll_down();
        ex.detail_scroll_down();
        assert_eq!(ex.detail_scroll, 6);
    }

    #[test]
    fn graph_explorer_detail_scroll_up_at_zero() {
        let mut ex = make_graph_explorer(vec![make_row("a", Status::Open)]);
        ex.detail_scroll = 0;
        ex.detail_scroll_up();
        assert_eq!(ex.detail_scroll, 0);
    }

    #[test]
    fn graph_explorer_toggle_view_mode() {
        let mut ex = make_graph_explorer(vec![]);
        assert_eq!(ex.view_mode, GraphViewMode::Tree);
        ex.toggle_view_mode();
        assert_eq!(ex.view_mode, GraphViewMode::Dag);
        ex.toggle_view_mode();
        assert_eq!(ex.view_mode, GraphViewMode::Tree);
    }

    #[test]
    fn graph_explorer_cycle_to_next_agent_task_empty() {
        let mut ex = make_graph_explorer(vec![make_row("a", Status::Open)]);
        // agent_active_indices is empty
        ex.cycle_to_next_agent_task();
        assert_eq!(ex.selected, 0); // unchanged
    }

    #[test]
    fn graph_explorer_cycle_to_next_agent_task_wraps() {
        let rows = vec![
            make_row("a", Status::Open),
            make_row("b", Status::InProgress),
            make_row("c", Status::Open),
            make_row("d", Status::InProgress),
        ];
        let mut ex = make_graph_explorer(rows);
        ex.agent_active_indices = vec![1, 3]; // b and d have agents
        ex.selected = 0;
        ex.cycle_to_next_agent_task();
        assert_eq!(ex.selected, 1);
        ex.cycle_to_next_agent_task();
        assert_eq!(ex.selected, 3);
        // Now at end — should wrap to first
        ex.cycle_to_next_agent_task();
        assert_eq!(ex.selected, 1);
    }

    fn make_graph_layout(task_ids: &[&str]) -> DagLayout {
        use super::super::graph_layout::LayoutNode;
        let nodes: Vec<LayoutNode> = task_ids
            .iter()
            .enumerate()
            .map(|(i, id)| LayoutNode {
                task_id: id.to_string(),
                title: id.to_string(),
                status: Status::Open,
                critical: false,
                active_agent_count: 0,
                active_agent_ids: vec![],
                layer: i,
                x: 0,
                y: i * 4,
                w: 10,
                h: 3,
            })
            .collect();
        let id_to_idx = task_ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.to_string(), i))
            .collect();
        DagLayout {
            nodes,
            edges: vec![],
            back_edges: vec![],
            loop_edges: vec![],
            width: 20,
            height: task_ids.len() * 4,
            id_to_idx,
            has_cycles: false,
        }
    }

    #[test]
    fn graph_explorer_dag_select_next_and_prev() {
        let mut ex = make_graph_explorer(vec![]);
        ex.graph_layout = Some(make_graph_layout(&["a", "b", "c"]));
        ex.dag_selected = 0;

        ex.dag_select_next();
        assert_eq!(ex.dag_selected, 1);
        ex.dag_select_next();
        assert_eq!(ex.dag_selected, 2);
        // Clamp at end
        ex.dag_select_next();
        assert_eq!(ex.dag_selected, 2);
        // Go back
        ex.dag_select_prev();
        assert_eq!(ex.dag_selected, 1);
        ex.dag_select_prev();
        assert_eq!(ex.dag_selected, 0);
        // Clamp at start
        ex.dag_select_prev();
        assert_eq!(ex.dag_selected, 0);
    }

    #[test]
    fn graph_explorer_dag_scroll_left_right() {
        let mut ex = make_graph_explorer(vec![]);
        assert_eq!(ex.dag_scroll_x, 0);
        ex.dag_scroll_right();
        assert_eq!(ex.dag_scroll_x, 4);
        ex.dag_scroll_right();
        assert_eq!(ex.dag_scroll_x, 8);
        ex.dag_scroll_left();
        assert_eq!(ex.dag_scroll_x, 4);
        ex.dag_scroll_left();
        assert_eq!(ex.dag_scroll_x, 0);
        // Saturating at 0
        ex.dag_scroll_left();
        assert_eq!(ex.dag_scroll_x, 0);
    }

    #[test]
    fn graph_explorer_dag_selected_task_id() {
        let mut ex = make_graph_explorer(vec![]);
        // No layout → None
        assert!(ex.dag_selected_task_id().is_none());

        ex.graph_layout = Some(make_graph_layout(&["my-task"]));
        ex.dag_selected = 0;
        assert_eq!(ex.dag_selected_task_id(), Some("my-task"));
    }

    #[test]
    fn graph_explorer_selected_task_first_agent_tree_mode() {
        let mut row = make_row("a", Status::InProgress);
        row.active_agent_count = 1;
        row.active_agent_ids = vec!["agent-x".to_string()];
        let mut ex = make_graph_explorer(vec![row]);
        ex.view_mode = GraphViewMode::Tree;
        assert_eq!(ex.selected_task_first_agent(), Some("agent-x".to_string()));
    }

    #[test]
    fn graph_explorer_selected_task_first_agent_tree_no_agents() {
        let ex = make_graph_explorer(vec![make_row("a", Status::Open)]);
        assert_eq!(ex.selected_task_first_agent(), None);
    }

    // ── LogViewer state ─────────────────────────────────────────────

    #[test]
    fn log_viewer_line_count() {
        let viewer = make_log_viewer(vec!["line1", "line2", "line3"]);
        assert_eq!(viewer.lines.len(), 3);
    }

    #[test]
    fn log_viewer_scroll_up_at_zero() {
        let mut viewer = make_log_viewer(vec!["a", "b", "c"]);
        viewer.scroll_offset = 0;
        viewer.scroll_up();
        assert_eq!(viewer.scroll_offset, 0);
    }

    #[test]
    fn log_viewer_scroll_up_decrements_and_disables_auto() {
        let mut viewer = make_log_viewer(vec!["a", "b", "c"]);
        viewer.scroll_offset = 2;
        viewer.auto_scroll = true;
        viewer.scroll_up();
        assert_eq!(viewer.scroll_offset, 1);
        assert!(!viewer.auto_scroll);
    }

    #[test]
    fn log_viewer_scroll_down_increments() {
        let mut viewer = make_log_viewer(vec!["a", "b", "c", "d", "e"]);
        viewer.scroll_offset = 0;
        viewer.scroll_down(3); // viewport_height=3, max_offset=5-3=2
        assert_eq!(viewer.scroll_offset, 1);
    }

    #[test]
    fn log_viewer_scroll_down_clamps_and_enables_auto() {
        let mut viewer = make_log_viewer(vec!["a", "b", "c", "d", "e"]);
        viewer.scroll_offset = 1;
        viewer.auto_scroll = false;
        viewer.scroll_down(3); // max_offset=2
        assert_eq!(viewer.scroll_offset, 2);
        assert!(viewer.auto_scroll); // re-enabled at bottom
    }

    #[test]
    fn log_viewer_scroll_down_already_at_max() {
        let mut viewer = make_log_viewer(vec!["a", "b"]);
        viewer.scroll_offset = 0;
        // viewport larger than lines → max_offset=0
        viewer.scroll_down(10);
        assert_eq!(viewer.scroll_offset, 0);
        assert!(viewer.auto_scroll);
    }

    #[test]
    fn log_viewer_page_up() {
        let mut viewer = make_log_viewer((0..50).map(|_| "line").collect());
        viewer.scroll_offset = 20;
        viewer.auto_scroll = true;
        viewer.page_up(20); // jump = 10
        assert_eq!(viewer.scroll_offset, 10);
        assert!(!viewer.auto_scroll);
    }

    #[test]
    fn log_viewer_page_up_saturates_at_zero() {
        let mut viewer = make_log_viewer((0..10).map(|_| "x").collect());
        viewer.scroll_offset = 3;
        viewer.page_up(20); // jump=10, saturating_sub → 0
        assert_eq!(viewer.scroll_offset, 0);
        assert!(!viewer.auto_scroll);
    }

    #[test]
    fn log_viewer_page_down() {
        let mut viewer = make_log_viewer((0..50).map(|_| "x").collect());
        viewer.scroll_offset = 0;
        viewer.auto_scroll = false;
        viewer.page_down(20); // jump=10, max_offset=30
        assert_eq!(viewer.scroll_offset, 10);
        assert!(!viewer.auto_scroll);
    }

    #[test]
    fn log_viewer_page_down_clamps_and_enables_auto() {
        let mut viewer = make_log_viewer((0..50).map(|_| "x").collect());
        viewer.scroll_offset = 28;
        viewer.auto_scroll = false;
        viewer.page_down(20); // jump=10, new=38, max_offset=30 → clamped to 30
        assert_eq!(viewer.scroll_offset, 30);
        assert!(viewer.auto_scroll);
    }

    #[test]
    fn log_viewer_apply_auto_scroll_when_enabled() {
        let mut viewer = make_log_viewer((0..50).map(|_| "x").collect());
        viewer.scroll_offset = 0;
        viewer.auto_scroll = true;
        viewer.apply_auto_scroll(20);
        assert_eq!(viewer.scroll_offset, 30); // 50 - 20
    }

    #[test]
    fn log_viewer_apply_auto_scroll_when_disabled() {
        let mut viewer = make_log_viewer((0..50).map(|_| "x").collect());
        viewer.scroll_offset = 5;
        viewer.auto_scroll = false;
        viewer.apply_auto_scroll(20);
        assert_eq!(viewer.scroll_offset, 5); // unchanged
    }

    // ── App panel switching ─────────────────────────────────────────

    #[test]
    fn app_toggle_panel() {
        let mut app = make_app();
        assert_eq!(app.selected_panel, Panel::Tasks);
        app.toggle_panel();
        assert_eq!(app.selected_panel, Panel::Agents);
        app.toggle_panel();
        assert_eq!(app.selected_panel, Panel::Tasks);
    }

    // ── App scroll up/down ──────────────────────────────────────────

    #[test]
    fn app_scroll_tasks_panel() {
        let mut app = make_app();
        app.tasks = sample_tasks();
        app.selected_panel = Panel::Tasks;
        app.task_selected = 0;

        app.scroll_down();
        assert_eq!(app.task_selected, 1);
        app.scroll_down();
        assert_eq!(app.task_selected, 2);
        // Clamp at end
        app.scroll_down();
        assert_eq!(app.task_selected, 2);
        // Go up
        app.scroll_up();
        assert_eq!(app.task_selected, 1);
        app.scroll_up();
        assert_eq!(app.task_selected, 0);
        // Saturate at 0
        app.scroll_up();
        assert_eq!(app.task_selected, 0);
    }

    #[test]
    fn app_scroll_agents_panel() {
        let mut app = make_app();
        app.agents = sample_agents();
        app.selected_panel = Panel::Agents;
        app.agent_selected = 0;

        app.scroll_down();
        assert_eq!(app.agent_selected, 1);
        // Clamp
        app.scroll_down();
        assert_eq!(app.agent_selected, 1);
        app.scroll_up();
        assert_eq!(app.agent_selected, 0);
        app.scroll_up();
        assert_eq!(app.agent_selected, 0);
    }

    #[test]
    fn app_scroll_empty_tasks_is_noop() {
        let mut app = make_app();
        app.selected_panel = Panel::Tasks;
        app.scroll_down();
        assert_eq!(app.task_selected, 0);
    }

    #[test]
    fn app_scroll_empty_agents_is_noop() {
        let mut app = make_app();
        app.selected_panel = Panel::Agents;
        app.scroll_down();
        assert_eq!(app.agent_selected, 0);
    }

    // ── App view transitions ────────────────────────────────────────

    #[test]
    fn app_close_log_viewer_returns_to_dashboard() {
        let mut app = make_app();
        app.view = View::LogView;
        app.log_viewer = Some(make_log_viewer(vec!["hello"]));
        app.close_log_viewer();
        assert_eq!(app.view, View::Dashboard);
        assert!(app.log_viewer.is_none());
    }

    #[test]
    fn app_close_graph_explorer_returns_to_dashboard() {
        let mut app = make_app();
        app.view = View::GraphExplorer;
        app.graph_explorer = Some(make_graph_explorer(vec![]));
        app.close_graph_explorer();
        assert_eq!(app.view, View::Dashboard);
        assert!(app.graph_explorer.is_none());
    }

    #[test]
    fn app_open_log_viewer_requires_agents_panel() {
        let mut app = make_app();
        app.selected_panel = Panel::Tasks;
        app.agents = sample_agents();
        app.open_log_viewer();
        // Should not open because panel is Tasks
        assert_eq!(app.view, View::Dashboard);
        assert!(app.log_viewer.is_none());
    }

    #[test]
    fn app_open_log_viewer_requires_agents() {
        let mut app = make_app();
        app.selected_panel = Panel::Agents;
        // No agents loaded
        app.open_log_viewer();
        assert_eq!(app.view, View::Dashboard);
        assert!(app.log_viewer.is_none());
    }

    // ── App help overlay toggle ─────────────────────────────────────

    #[test]
    fn app_help_toggle() {
        let mut app = make_app();
        assert!(!app.show_help);
        app.show_help = true;
        assert!(app.show_help);
        app.show_help = false;
        assert!(!app.show_help);
    }

    // ── App view_label and key_hints ────────────────────────────────

    #[test]
    fn app_view_label() {
        let mut app = make_app();
        assert_eq!(app.view_label(), "Dashboard");
        app.view = View::LogView;
        assert_eq!(app.view_label(), "Log Viewer");
        app.view = View::GraphExplorer;
        assert_eq!(app.view_label(), "Graph Explorer");
    }

    #[test]
    fn app_key_hints_per_view() {
        let mut app = make_app();
        assert!(app.key_hints().contains("quit"));
        app.view = View::LogView;
        assert!(app.key_hints().contains("scroll"));
        app.view = View::GraphExplorer;
        assert!(app.key_hints().contains("toggle view"));
    }

    // ── AgentInfo helpers ───────────────────────────────────────────

    #[test]
    fn agent_info_is_alive() {
        let mut info = AgentInfo {
            id: "a".into(),
            task_id: "t".into(),
            executor: "e".into(),
            pid: 1,
            uptime: "1s".into(),
            status: AgentStatus::Working,
            output_file: String::new(),
        };
        assert!(info.is_alive());
        info.status = AgentStatus::Starting;
        assert!(info.is_alive());
        info.status = AgentStatus::Idle;
        assert!(info.is_alive());
        info.status = AgentStatus::Stopping;
        assert!(!info.is_alive());
        info.status = AgentStatus::Done;
        assert!(!info.is_alive());
        info.status = AgentStatus::Dead;
        assert!(!info.is_alive());
        info.status = AgentStatus::Failed;
        assert!(!info.is_alive());
    }

    #[test]
    fn agent_info_is_dead() {
        let mut info = AgentInfo {
            id: "a".into(),
            task_id: "t".into(),
            executor: "e".into(),
            pid: 1,
            uptime: "1s".into(),
            status: AgentStatus::Dead,
            output_file: String::new(),
        };
        assert!(info.is_dead());
        info.status = AgentStatus::Failed;
        assert!(info.is_dead());
        info.status = AgentStatus::Working;
        assert!(!info.is_dead());
        info.status = AgentStatus::Done;
        assert!(!info.is_dead());
    }

    // ── App drill_in ────────────────────────────────────────────────

    #[test]
    fn app_drill_in_agents_panel_no_agents_is_noop() {
        let mut app = make_app();
        app.selected_panel = Panel::Agents;
        app.drill_in();
        assert_eq!(app.view, View::Dashboard);
    }

    // ── Panel and View equality ─────────────────────────────────────

    #[test]
    fn panel_eq() {
        assert_eq!(Panel::Tasks, Panel::Tasks);
        assert_eq!(Panel::Agents, Panel::Agents);
        assert_ne!(Panel::Tasks, Panel::Agents);
    }

    #[test]
    fn view_eq() {
        assert_eq!(View::Dashboard, View::Dashboard);
        assert_eq!(View::LogView, View::LogView);
        assert_eq!(View::GraphExplorer, View::GraphExplorer);
        assert_ne!(View::Dashboard, View::LogView);
    }

    #[test]
    fn graph_view_mode_eq() {
        assert_eq!(GraphViewMode::Tree, GraphViewMode::Tree);
        assert_eq!(GraphViewMode::Dag, GraphViewMode::Dag);
        assert_ne!(GraphViewMode::Tree, GraphViewMode::Dag);
    }
}
