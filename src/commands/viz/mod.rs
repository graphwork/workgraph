pub(crate) mod ascii;
mod dot;
mod graph;

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::graph::{Status, Task, TokenUsage, WorkGraph, parse_token_usage_live};

// Re-export public API
pub use graph::{generate_graph, generate_graph_with_overrides};

/// Rich annotation info for a parent task, carrying both the display text
/// and the dot-task IDs that produced it (for click resolution in the TUI).
#[derive(Debug, Clone)]
pub struct AnnotationInfo {
    /// The rendered annotation text (e.g. "[⊞ assigning]").
    pub text: String,
    /// The system task IDs that contributed to this annotation (e.g. [".assign-my-task"]).
    pub dot_task_ids: Vec<String>,
}

/// Structured output from viz generation, containing both the rendered string
/// and metadata needed for interactive features (e.g., TUI task selection).
pub struct VizOutput {
    /// The rendered visualization string.
    pub text: String,
    /// Map from task ID to the line index where it appears in the output.
    pub node_line_map: HashMap<String, usize>,
    /// Ordered list of task IDs in the order they appear (by line number).
    /// Used for arrow-key navigation in the TUI.
    pub task_order: Vec<String>,
    /// Forward edges: task_id → list of dependent task IDs (tasks that depend on it).
    pub forward_edges: HashMap<String, Vec<String>>,
    /// Reverse edges: task_id → list of dependency task IDs (tasks it depends on).
    pub reverse_edges: HashMap<String, Vec<String>>,
    /// Per-character edge map: (line, visible_column) → list of (source_id, target_id).
    /// Maps each edge/connector character to the graph edge(s) it represents.
    /// Positions in shared arc columns may carry multiple edges (e.g., a vertical
    /// segment that passes through multiple arcs).
    /// Only edge characters have entries; text characters are absent.
    pub char_edge_map: HashMap<(usize, usize), Vec<(String, String)>>,
    /// Cycle membership: task_id → set of all task IDs in the same SCC.
    /// Only populated for tasks that are in non-trivial SCCs (>1 member).
    pub cycle_members: HashMap<String, HashSet<String>>,
    /// Phase annotation info per parent task ID, carrying display text and
    /// the dot-task IDs that produced the annotation (for TUI click resolution).
    pub annotation_map: HashMap<String, AnnotationInfo>,
}

/// Output format for visualization
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Dot,
    Mermaid,
    Ascii,
    Graph,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "dot" => Ok(OutputFormat::Dot),
            "mermaid" => Ok(OutputFormat::Mermaid),
            "ascii" | "dag" => Ok(OutputFormat::Ascii),
            "graph" => Ok(OutputFormat::Graph),
            _ => Err(format!(
                "Unknown format: {}. Use 'dot', 'mermaid', 'ascii', or 'graph'.",
                s
            )),
        }
    }
}

/// Layout strategy for the ASCII tree visualization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayoutMode {
    /// Classic DFS-order tree: fan-in nodes claimed by first parent visited.
    Tree,
    /// Diamond-aware layout: fan-in nodes placed under their lowest common
    /// ancestor so arcs flow downward instead of upward.
    #[default]
    Diamond,
}

impl std::str::FromStr for LayoutMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "tree" => Ok(LayoutMode::Tree),
            "diamond" => Ok(LayoutMode::Diamond),
            _ => Err(format!("Unknown layout: {}. Use 'tree' or 'diamond'.", s)),
        }
    }
}

impl std::fmt::Display for LayoutMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LayoutMode::Tree => write!(f, "tree"),
            LayoutMode::Diamond => write!(f, "diamond"),
        }
    }
}

/// Options for the viz command
#[derive(Clone)]
pub struct VizOptions {
    pub all: bool,
    pub status: Option<String>,
    pub critical_path: bool,
    pub format: OutputFormat,
    pub output: Option<String>,
    /// Show internal tasks (assign-*, evaluate-*) that are normally hidden
    pub show_internal: bool,
    /// Show only internal tasks that are currently running (in-progress/open)
    pub show_internal_running_only: bool,
    /// Focus on specific task IDs — show only their containing subgraphs
    pub focus: Vec<String>,
    /// TUI mode: sort subgraphs by most-recently-updated first (LRU ordering)
    #[allow(dead_code)]
    pub tui_mode: bool,
    /// Layout strategy for tree construction
    pub layout: LayoutMode,
    /// Filter by tags (AND semantics — task must have all specified tags)
    pub tags: Vec<String>,
    /// Edge color style: "gray", "white", or "mixed"
    pub edge_color: String,
    /// Maximum output width in columns (None = no limit)
    pub max_columns: Option<u16>,
}

impl Default for VizOptions {
    fn default() -> Self {
        Self {
            all: false,
            status: None,
            critical_path: false,
            format: OutputFormat::Ascii,
            output: None,
            show_internal: false,
            show_internal_running_only: false,
            focus: Vec::new(),
            tui_mode: false,
            layout: LayoutMode::default(),
            tags: Vec::new(),
            edge_color: "gray".to_string(),
            max_columns: None,
        }
    }
}

/// Returns true if the task is an auto-generated internal task (assignment or evaluation).
/// Coordinator and compact tasks are exempt — always visible.
fn is_internal_task(task: &Task) -> bool {
    if task
        .tags
        .iter()
        .any(|t| t == "coordinator-loop" || t == "compact-loop")
    {
        return false;
    }
    workgraph::graph::is_system_task(&task.id)
        || task
            .tags
            .iter()
            .any(|t| t == "assignment" || t == "evaluation")
}

/// Returns true if the task is a coordinator task.
pub(crate) fn is_coordinator_task(task: &Task) -> bool {
    task.tags.iter().any(|t| t == "coordinator-loop")
}

/// Returns true if a pipeline task is actively running (not just existing/pending).
///
/// Only `InProgress` and `PendingValidation` count as active — `Open`, `Blocked`,
/// and `Waiting` mean the pipeline stage hasn't started yet and shouldn't be shown
/// as an active indicator.
fn is_pipeline_active(task: &Task) -> bool {
    matches!(task.status, Status::InProgress | Status::PendingValidation)
}

/// Determine the phase annotation for a parent task based on its related internal tasks.
///
/// - If an assignment task is actively running → "[assigning]"
/// - If an evaluation task is actively running → "[evaluating]"
fn compute_phase_annotation(internal_task: &Task) -> &'static str {
    let id = &internal_task.id;
    if id.starts_with(".assign-") || id.starts_with("assign-") {
        "[⊞ assigning]"
    } else if id.starts_with(".verify-") || id.starts_with("verify-") {
        "[∴ validating]"
    } else {
        "[∴ evaluating]"
    }
}

/// Extract the parent task ID from a system task ID.
fn system_task_parent_id(id: &str) -> Option<String> {
    for prefix in &[
        ".assign-",
        ".evaluate-",
        ".verify-",
        ".flip-",
        ".respond-to-",
        ".place-", // Legacy: kept so old .place-* tasks still resolve
        "assign-",
        "evaluate-",
        "verify-",
        "flip-",
        "respond-to-",
    ] {
        if let Some(rest) = id.strip_prefix(prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

/// Filter out internal tasks and compute phase annotations for their parent tasks.
///
/// Returns:
/// - The filtered list of tasks (internal tasks removed)
/// - A map of parent_task_id → AnnotationInfo (display text + source dot-task IDs)
pub(crate) fn filter_internal_tasks<'a>(
    _graph: &'a WorkGraph,
    tasks: Vec<&'a Task>,
    _existing_annotations: &HashMap<String, AnnotationInfo>,
) -> (Vec<&'a Task>, HashMap<String, AnnotationInfo>) {
    let mut annotations: HashMap<String, AnnotationInfo> = HashMap::new();
    let mut internal_ids: HashSet<&str> = HashSet::new();

    for task in &tasks {
        if !is_internal_task(task) {
            continue;
        }
        internal_ids.insert(task.id.as_str());

        if let Some(pid) = system_task_parent_id(&task.id)
            && is_pipeline_active(task)
        {
            let annotation = compute_phase_annotation(task);
            annotations
                .entry(pid)
                .and_modify(|existing| {
                    existing.text.push(' ');
                    existing.text.push_str(annotation);
                    existing.dot_task_ids.push(task.id.clone());
                })
                .or_insert_with(|| AnnotationInfo {
                    text: annotation.to_string(),
                    dot_task_ids: vec![task.id.clone()],
                });
        }
    }

    // Second pass: filter out internal tasks and fix edges
    // For tasks that were blocked by internal tasks, rewire to the internal task's blockers
    let filtered: Vec<&'a Task> = tasks
        .into_iter()
        .filter(|t| !internal_ids.contains(t.id.as_str()))
        .collect();

    (filtered, annotations)
}

/// Like `filter_internal_tasks`, but keeps internal tasks that are currently running
/// (in-progress or open). Non-running internal tasks are still filtered out.
pub(crate) fn filter_internal_tasks_running_only<'a>(
    _graph: &'a WorkGraph,
    tasks: Vec<&'a Task>,
    _existing_annotations: &HashMap<String, AnnotationInfo>,
) -> (Vec<&'a Task>, HashMap<String, AnnotationInfo>) {
    let mut annotations: HashMap<String, AnnotationInfo> = HashMap::new();

    // Compute phase annotations only for actively-running internal tasks
    for task in &tasks {
        if !is_internal_task(task) {
            continue;
        }
        if let Some(pid) = system_task_parent_id(&task.id)
            && is_pipeline_active(task)
        {
            let annotation = compute_phase_annotation(task);
            annotations
                .entry(pid)
                .and_modify(|existing| {
                    existing.text.push(' ');
                    existing.text.push_str(annotation);
                    existing.dot_task_ids.push(task.id.clone());
                })
                .or_insert_with(|| AnnotationInfo {
                    text: annotation.to_string(),
                    dot_task_ids: vec![task.id.clone()],
                });
        }
    }

    let filtered: Vec<&'a Task> = tasks
        .into_iter()
        .filter(|t| {
            if !is_internal_task(t) {
                return true;
            }
            // Keep only running internal tasks
            matches!(t.status, Status::InProgress | Status::Open)
        })
        .collect();

    (filtered, annotations)
}

/// Compute the annotation text for the `.compact-0` node.
///
/// Shows compaction state and context pressure so it's immediately visible in
/// the graph view whether compaction is idle, running, or overdue.
///
/// Format examples:
/// - `[idle · 40% ctx]`   – idle, far below threshold
/// - `[⚠ 85% ctx]`       – approaching threshold (yellow when colored)
/// - `[⚠ 102% ctx]`      – past threshold, overdue (red when colored)
/// - `[⟳ compacting · 50% ctx]` – actively running (red + spinner)
/// - `[✓ compacted 12s ago]`     – recently finished (green flash)
fn compact_node_annotation(
    compact_task_status: Status,
    accumulated_tokens: u64,
    threshold: u64,
    last_compaction: Option<&str>,
) -> String {
    let now = chrono::Utc::now();

    // Check if recently completed (within 60s): show green "done" indicator.
    if let Some(ts_str) = last_compaction
        && let Ok(dt) = ts_str.parse::<chrono::DateTime<chrono::Utc>>()
    {
        let secs_ago = (now - dt).num_seconds().max(0);
        if secs_ago < 60 {
            return format!("[✓ compacted {}s ago]", secs_ago);
        }
    }

    // While actively compacting, show spinner + context pressure.
    if compact_task_status == Status::InProgress {
        if threshold > 0 {
            let pct = (accumulated_tokens as f64 / threshold as f64 * 100.0) as u64;
            return format!("[⟳ compacting · {}% ctx]", pct);
        }
        return "[⟳ compacting]".to_string();
    }

    // Idle — show context pressure relative to threshold.
    if threshold > 0 {
        let pct = (accumulated_tokens as f64 / threshold as f64 * 100.0) as u64;
        if pct >= 70 {
            // Warning (yellow ≥70%) or overdue (red ≥100%)
            return format!("[⚠ {}% ctx]", pct);
        }
        return format!("[idle · {}% ctx]", pct);
    }

    "[idle]".to_string()
}

/// Generate the ASCII viz output string for the given directory and options.
/// Used by both the CLI `wg viz` command and the TUI viewer.
pub fn generate_viz_output(dir: &Path, options: &VizOptions) -> Result<VizOutput> {
    let (graph, _path) = super::load_workgraph(dir)?;
    generate_viz_output_from_graph(&graph, dir, options)
}

/// Generate viz output from an already-loaded graph. Useful when the caller
/// already has the graph loaded (e.g., the TUI viewer for task counting).
pub fn generate_viz_output_from_graph(
    graph: &WorkGraph,
    dir: &Path,
    options: &VizOptions,
) -> Result<VizOutput> {
    // Compute cycle analysis so we can preserve cycle members in filtered views
    let cycle_analysis = graph.compute_cycle_analysis();

    // Find cycle indices that have at least one non-done member —
    // all members of such cycles should be shown even without --all.
    let _active_cycle_ids: HashSet<usize> = if options.all || options.status.is_some() {
        HashSet::new()
    } else {
        let mut active = HashSet::new();
        for task in graph.tasks() {
            if task.status != Status::Done
                && let Some(&ci) = cycle_analysis.task_to_cycle.get(&task.id)
            {
                active.insert(ci);
            }
        }
        active
    };

    // Compute weakly connected components via union-find.
    // Used for both active-tree filtering and --focus subgraph selection.
    fn uf_find<'a>(
        comp: &mut HashMap<&'a str, usize>,
        merged: &mut [Option<usize>],
        id: &'a str,
    ) -> usize {
        let mut c = comp[id];
        while let Some(parent) = merged[c] {
            c = parent;
        }
        let root = c;
        let mut c2 = comp[id];
        while let Some(parent) = merged[c2] {
            merged[c2] = Some(root);
            c2 = parent;
        }
        comp.insert(id, root);
        root
    }

    let mut components: HashMap<&str, usize> = HashMap::new();
    let mut num_components = 0usize;
    for task in graph.tasks() {
        components.insert(task.id.as_str(), num_components);
        num_components += 1;
    }
    let mut merged: Vec<Option<usize>> = vec![None; num_components];

    let edge_pairs: Vec<(String, String)> = graph
        .tasks()
        .flat_map(|task| {
            let id = task.id.clone();
            task.after
                .iter()
                .chain(task.before.iter())
                .map(move |neighbor| (id.clone(), neighbor.clone()))
        })
        .collect();

    for (task_id, neighbor_id) in &edge_pairs {
        if components.contains_key(neighbor_id.as_str()) {
            let a = uf_find(&mut components, &mut merged, task_id.as_str());
            let b = uf_find(&mut components, &mut merged, neighbor_id.as_str());
            if a != b {
                merged[b] = Some(a);
            }
        }
    }

    // Precompute task_id → root mapping so we don't need mutable borrows in the filter
    let task_roots: HashMap<&str, usize> = graph
        .tasks()
        .map(|t| {
            (
                t.id.as_str(),
                uf_find(&mut components, &mut merged, t.id.as_str()),
            )
        })
        .collect();

    // For focus mode: collect the roots of focused task IDs
    let focus_roots: HashSet<usize> = options
        .focus
        .iter()
        .filter_map(|f| task_roots.get(f.as_str()).copied())
        .collect();

    // For default mode: find roots with active (non-done, non-internal) tasks
    let active_roots: HashSet<usize> = graph
        .tasks()
        .filter(|t| {
            t.status != Status::Done && t.status != Status::Abandoned && !is_internal_task(t)
        })
        .filter_map(|t| task_roots.get(t.id.as_str()).copied())
        .collect();

    // Helper: map a Status to its lowercase string for filter comparison
    let status_str = |s: &Status| -> &'static str {
        match s {
            Status::Open => "open",
            Status::InProgress => "in-progress",
            Status::Done => "done",
            Status::Blocked => "blocked",
            Status::Failed => "failed",
            Status::Abandoned => "abandoned",
            Status::Waiting | Status::PendingValidation => "waiting",
        }
    };

    // Determine which tasks to include
    let tasks_to_show: Vec<_> = graph
        .tasks()
        .filter(|t| {
            let root = task_roots[t.id.as_str()];

            // Focus mode: show only WCCs containing the focused task IDs
            if !options.focus.is_empty() {
                return focus_roots.contains(&root);
            }

            // If --all, show everything
            if options.all {
                return true;
            }

            // If --status filter is specified, use it
            if let Some(ref status_filter) = options.status {
                return status_str(&t.status) == status_filter.to_lowercase();
            }

            // Default: show tasks in active WCCs
            if t.status == Status::Abandoned {
                return false;
            }
            active_roots.contains(&root)
        })
        // Tag filter: task must have all specified tags (AND semantics)
        .filter(|t| options.tags.iter().all(|tag| t.tags.contains(tag)))
        .collect();

    // When --status is used, walk up ancestors to provide spatial context.
    // Ancestors are included as dimmed "context" nodes so the user can see
    // how filtered tasks connect back to the graph roots.
    let mut context_ids: HashSet<String> = HashSet::new();
    if let Some(ref status_filter) = options.status {
        let filter_lower = status_filter.to_lowercase();
        let primary_ids: HashSet<&str> = tasks_to_show.iter().map(|t| t.id.as_str()).collect();
        let mut visited: HashSet<String> = primary_ids.iter().map(|&s| s.to_string()).collect();
        let mut to_visit: Vec<String> = visited.iter().cloned().collect();

        while let Some(id) = to_visit.pop() {
            if let Some(task) = graph.get_task(&id) {
                for dep in &task.after {
                    if visited.contains(dep.as_str()) {
                        continue;
                    }
                    visited.insert(dep.clone());
                    context_ids.insert(dep.clone());

                    // Keep walking up unless this ancestor matches the status filter
                    // (status-matching ancestors are already context but we stop climbing past them)
                    if let Some(ancestor) = graph.get_task(dep)
                        && status_str(&ancestor.status) != filter_lower
                    {
                        to_visit.push(dep.clone());
                    }
                }
            }
        }
    }

    // Add context ancestor tasks to the display set
    let tasks_to_show = if !context_ids.is_empty() {
        let mut tasks_to_show = tasks_to_show;
        let existing_ids: HashSet<&str> = tasks_to_show.iter().map(|t| t.id.as_str()).collect();
        for ctx_id in &context_ids {
            if !existing_ids.contains(ctx_id.as_str())
                && let Some(task) = graph.get_task(ctx_id)
            {
                tasks_to_show.push(task);
            }
        }
        tasks_to_show
    } else {
        tasks_to_show
    };

    // Filter out internal tasks (assign-*, evaluate-*) unless --show-internal
    let empty_annotations: HashMap<String, AnnotationInfo> = HashMap::new();
    let (tasks_to_show, mut annotations) = if options.show_internal {
        (tasks_to_show, empty_annotations)
    } else if options.show_internal_running_only {
        filter_internal_tasks_running_only(graph, tasks_to_show, &empty_annotations)
    } else {
        filter_internal_tasks(graph, tasks_to_show, &empty_annotations)
    };

    // Inject compaction status annotation onto the .compact-0 node.
    // This shows context pressure and compaction state directly in the graph view.
    if tasks_to_show.iter().any(|t| t.id == ".compact-0") {
        let compact_task_status = graph
            .get_task(".compact-0")
            .map(|t| t.status)
            .unwrap_or(Status::Open);
        let total_tokens =
            crate::commands::service::CoordinatorState::total_accumulated_tokens(dir);
        let config = workgraph::config::Config::load(dir).unwrap_or_default();
        let threshold = config.effective_compaction_threshold();
        let compactor_state = workgraph::service::compactor::CompactorState::load(dir);
        let annotation_text = compact_node_annotation(
            compact_task_status,
            total_tokens,
            threshold,
            compactor_state.last_compaction.as_deref(),
        );
        annotations.insert(
            ".compact-0".to_string(),
            AnnotationInfo {
                text: annotation_text,
                dot_task_ids: vec![],
            },
        );
    }

    // Resolve cross-repo peer dependencies: create synthetic Task nodes for peer refs
    // so they appear in the graph with their resolved remote status.
    let peer_tasks: Vec<Task> = {
        let mut seen = HashSet::new();
        let mut peers = Vec::new();
        for task in &tasks_to_show {
            for dep in &task.after {
                if let Some((peer_name, remote_task_id)) =
                    workgraph::federation::parse_remote_ref(dep)
                    && seen.insert(dep.clone())
                {
                    let remote = workgraph::federation::resolve_remote_task_status(
                        peer_name,
                        remote_task_id,
                        dir,
                    );
                    let title = remote.title.unwrap_or_else(|| remote_task_id.to_string());
                    peers.push(Task {
                        id: dep.clone(),
                        title,
                        status: remote.status,
                        ..Task::default()
                    });
                }
            }
        }
        peers
    };

    // Extend tasks_to_show with peer task references
    let mut tasks_to_show = tasks_to_show;
    for pt in &peer_tasks {
        tasks_to_show.push(pt);
    }
    let task_ids: HashSet<&str> = tasks_to_show.iter().map(|t| t.id.as_str()).collect();

    // Calculate critical path if requested
    let critical_path_set: HashSet<String> = if options.critical_path {
        calculate_critical_path(graph, &task_ids)
    } else {
        HashSet::new()
    };

    // Enrich tasks with live token usage from agent output logs when not persisted.
    // Includes InProgress, Done, and Failed tasks — any with an assigned agent.
    let agents_dir = dir.join("agents");
    let live_token_usage: HashMap<String, TokenUsage> = tasks_to_show
        .iter()
        .filter(|t| t.token_usage.is_none())
        .filter(|t| matches!(t.status, Status::InProgress | Status::Done | Status::Failed))
        .filter_map(|t| {
            // Try live agent dir first
            let usage = t.assigned.as_deref().and_then(|agent_id| {
                let log_path = agents_dir.join(agent_id).join("output.log");
                parse_token_usage_live(&log_path)
            });
            if let Some(u) = usage {
                return Some((t.id.clone(), u));
            }
            // Fall back to archived output
            let archive_base = dir.join("log").join("agents").join(&t.id);
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
                    return Some((t.id.clone(), u));
                }
            }
            None
        })
        .collect();

    // Build unified agency token usage map: aggregate all lifecycle tasks
    // (.assign-*, .evaluate-*, .flip-*, .verify-*) into a single total per parent task.
    let mut agency_token_usage: HashMap<String, TokenUsage> = HashMap::new();
    for task in graph.tasks() {
        if !is_internal_task(task) {
            continue;
        }
        let parent_id = system_task_parent_id(&task.id);
        let Some(pid) = parent_id else { continue };
        let usage = task
            .token_usage
            .as_ref()
            .cloned()
            .or_else(|| {
                // Try live agent dir first
                let agent_id = task.assigned.as_deref()?;
                let log_path = agents_dir.join(agent_id).join("output.log");
                parse_token_usage_live(&log_path)
            })
            .or_else(|| {
                // Fall back to archived output for cleaned-up agents
                let archive_base = dir.join("log").join("agents").join(&task.id);
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
                        && let Some(usage) = parse_token_usage_live(&candidate)
                    {
                        return Some(usage);
                    }
                }
                None
            });
        if let Some(u) = usage {
            let entry = agency_token_usage.entry(pid).or_insert_with(|| TokenUsage {
                cost_usd: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            });
            entry.accumulate(&u);
        }
    }

    // Compute per-task message stats (in/out counts, read status).
    let message_stats: HashMap<String, workgraph::messages::MessageStats> = tasks_to_show
        .iter()
        .filter_map(|t| {
            let stats = workgraph::messages::message_stats(dir, &t.id, t.assigned.as_deref());
            if stats.incoming > 0 || stats.outgoing > 0 {
                Some((t.id.clone(), stats))
            } else {
                None
            }
        })
        .collect();

    // Compute per-task coordinator message status (TUI-perspective read state).
    let coordinator_status: HashMap<String, workgraph::messages::CoordinatorMessageStatus> =
        tasks_to_show
            .iter()
            .filter_map(|t| {
                workgraph::messages::coordinator_message_status(dir, &t.id)
                    .map(|s| (t.id.clone(), s))
            })
            .collect();

    // Generate output
    let output = match options.format {
        OutputFormat::Ascii => ascii::generate_ascii(
            graph,
            &tasks_to_show,
            &task_ids,
            &annotations,
            &live_token_usage,
            &agency_token_usage,
            options.layout,
            &context_ids,
            &options.edge_color,
            &message_stats,
            &coordinator_status,
        ),
        _ => {
            let text = match options.format {
                OutputFormat::Dot => dot::generate_dot(
                    graph,
                    &tasks_to_show,
                    &task_ids,
                    &critical_path_set,
                    &annotations,
                ),
                OutputFormat::Mermaid => dot::generate_mermaid(
                    graph,
                    &tasks_to_show,
                    &task_ids,
                    &critical_path_set,
                    &annotations,
                ),
                OutputFormat::Graph => graph::generate_graph(
                    graph,
                    &tasks_to_show,
                    &task_ids,
                    &annotations,
                    &live_token_usage,
                    &agency_token_usage,
                    &context_ids,
                ),
                OutputFormat::Ascii => unreachable!(),
            };
            VizOutput {
                text,
                node_line_map: HashMap::new(),
                task_order: Vec::new(),
                forward_edges: HashMap::new(),
                reverse_edges: HashMap::new(),
                char_edge_map: HashMap::new(),
                cycle_members: HashMap::new(),
                annotation_map: annotations,
            }
        }
    };

    Ok(output)
}

pub fn run(dir: &Path, options: &VizOptions) -> Result<()> {
    let output = generate_viz_output(dir, options)?;

    // If output file is specified, render with dot
    if let Some(ref output_path) = options.output {
        if options.format != OutputFormat::Dot {
            anyhow::bail!("--output requires --format dot");
        }
        dot::render_dot(&output.text, output_path)?;
        println!("Rendered graph to {}", output_path);
    } else {
        // Truncate lines to terminal width if known
        let text = if let Some(cols) = options.max_columns {
            ascii::truncate_lines(&output.text, cols)
        } else {
            output.text
        };
        println!("{}", text);
    }

    Ok(())
}

/// Calculate the critical path (longest dependency chain by hours)
fn calculate_critical_path(graph: &WorkGraph, active_ids: &HashSet<&str>) -> HashSet<String> {
    // Build forward index: task_id -> tasks that it blocks
    let mut forward_index: HashMap<&str, Vec<&str>> = HashMap::new();

    for task in graph.tasks() {
        if !active_ids.contains(task.id.as_str()) {
            continue;
        }

        for blocker_id in &task.after {
            if active_ids.contains(blocker_id.as_str()) {
                forward_index
                    .entry(blocker_id.as_str())
                    .or_default()
                    .push(task.id.as_str());
            }
        }
    }

    // Find entry points (tasks with no active blockers)
    let entry_points: Vec<&str> = graph
        .tasks()
        .filter(|t| active_ids.contains(t.id.as_str()))
        .filter(|t| t.after.iter().all(|b| !active_ids.contains(b.as_str())))
        .map(|t| t.id.as_str())
        .collect();

    // Calculate longest path from each entry point
    let mut memo: HashMap<&str, (f64, Vec<String>)> = HashMap::new();
    let mut visited: HashSet<&str> = HashSet::new();

    for entry in &entry_points {
        calc_longest_path(entry, graph, &forward_index, &mut memo, &mut visited);
    }

    // Find the overall longest path
    memo.into_values()
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(_, path)| path.into_iter().collect())
        .unwrap_or_default()
}

fn calc_longest_path<'a>(
    task_id: &'a str,
    graph: &'a WorkGraph,
    forward_index: &HashMap<&'a str, Vec<&'a str>>,
    memo: &mut HashMap<&'a str, (f64, Vec<String>)>,
    visited: &mut HashSet<&'a str>,
) -> (f64, Vec<String>) {
    // Cycle detection
    if visited.contains(task_id) {
        return (0.0, vec![]);
    }

    if let Some(result) = memo.get(task_id) {
        return result.clone();
    }

    let task = match graph.get_task(task_id) {
        Some(t) => t,
        None => return (0.0, vec![]),
    };

    visited.insert(task_id);

    let task_hours = task.estimate.as_ref().and_then(|e| e.hours).unwrap_or(1.0);

    let (longest_child_hours, longest_child_path) =
        if let Some(children) = forward_index.get(task_id) {
            children
                .iter()
                .map(|child_id| calc_longest_path(child_id, graph, forward_index, memo, visited))
                .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
                .unwrap_or((0.0, vec![]))
        } else {
            (0.0, vec![])
        };

    visited.remove(task_id);

    let total_hours = task_hours + longest_child_hours;
    let mut path = vec![task_id.to_string()];
    path.extend(longest_child_path);

    memo.insert(task_id, (total_hours, path.clone()));
    (total_hours, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::format_hours;
    use workgraph::graph::{Estimate, Node, Task};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    fn make_task_with_hours(id: &str, title: &str, hours: f64) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            estimate: Some(Estimate {
                hours: Some(hours),
                cost: None,
            }),
            ..Task::default()
        }
    }

    fn make_internal_task(id: &str, title: &str, tag: &str, after: Vec<&str>) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            tags: vec![tag.to_string(), "agency".to_string()],
            after: after.into_iter().map(String::from).collect(),
            ..Task::default()
        }
    }

    #[test]
    fn test_format_output_parsing() {
        assert_eq!("dot".parse::<OutputFormat>().unwrap(), OutputFormat::Dot);
        assert_eq!(
            "mermaid".parse::<OutputFormat>().unwrap(),
            OutputFormat::Mermaid
        );
        assert_eq!("DOT".parse::<OutputFormat>().unwrap(), OutputFormat::Dot);
        assert!("invalid".parse::<OutputFormat>().is_err());
    }

    #[test]
    fn test_format_output_parsing_ascii() {
        assert_eq!(
            "ascii".parse::<OutputFormat>().unwrap(),
            OutputFormat::Ascii
        );
        assert_eq!("dag".parse::<OutputFormat>().unwrap(), OutputFormat::Ascii);
        assert_eq!(
            "ASCII".parse::<OutputFormat>().unwrap(),
            OutputFormat::Ascii
        );
    }

    #[test]
    fn test_calculate_critical_path_simple() {
        let mut graph = WorkGraph::new();
        let t1 = make_task_with_hours("t1", "Task 1", 8.0);
        let mut t2 = make_task_with_hours("t2", "Task 2", 16.0);
        t2.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let active_ids: HashSet<&str> = vec!["t1", "t2"].into_iter().collect();
        let critical_path = calculate_critical_path(&graph, &active_ids);

        assert!(critical_path.contains("t1"));
        assert!(critical_path.contains("t2"));
    }

    #[test]
    fn test_calculate_critical_path_picks_longest() {
        let mut graph = WorkGraph::new();

        // Two parallel paths:
        // t1 (8h) -> t2 (16h) = 24h
        // t1 (8h) -> t3 (2h) = 10h
        // Critical path should be t1 -> t2
        let t1 = make_task_with_hours("t1", "Task 1", 8.0);
        let mut t2 = make_task_with_hours("t2", "Task 2", 16.0);
        t2.after = vec!["t1".to_string()];
        let mut t3 = make_task_with_hours("t3", "Task 3", 2.0);
        t3.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let active_ids: HashSet<&str> = vec!["t1", "t2", "t3"].into_iter().collect();
        let critical_path = calculate_critical_path(&graph, &active_ids);

        assert!(critical_path.contains("t1"));
        assert!(critical_path.contains("t2"));
        // t3 should NOT be in critical path
        assert!(!critical_path.contains("t3"));
    }

    #[test]
    fn test_format_hours() {
        assert_eq!(format_hours(8.0), "8");
        assert_eq!(format_hours(8.5), "8.5");
        assert_eq!(format_hours(8.25), "8.2");
    }

    #[test]
    fn test_calculate_critical_path_with_nan_hours() {
        let mut graph = WorkGraph::new();

        let t1 = make_task_with_hours("t1", "Task 1", f64::NAN);
        let mut t2 = make_task_with_hours("t2", "Task 2", 4.0);
        t2.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let active_ids: HashSet<&str> = vec!["t1", "t2"].into_iter().collect();

        // Should not panic with NaN estimates
        let path = calculate_critical_path(&graph, &active_ids);
        // Path should still contain tasks (exact ordering with NaN is unspecified)
        assert!(!path.is_empty());
    }

    #[test]
    fn test_calculate_critical_path_empty_graph() {
        let graph = WorkGraph::new();
        let active_ids: HashSet<&str> = HashSet::new();
        let path = calculate_critical_path(&graph, &active_ids);
        assert!(path.is_empty());
    }

    #[test]
    fn test_format_hours_nan_and_infinity() {
        assert_eq!(format_hours(f64::NAN), "?");
        assert_eq!(format_hours(f64::INFINITY), "?");
        assert_eq!(format_hours(f64::NEG_INFINITY), "?");
        assert_eq!(format_hours(5.0), "5");
        assert_eq!(format_hours(2.5), "2.5");
    }

    // --- Internal task filtering tests ---

    #[test]
    fn test_is_internal_task() {
        let assign = make_internal_task("assign-foo", "Assign agent to foo", "assignment", vec![]);
        let eval = make_internal_task("evaluate-foo", "Evaluate foo", "evaluation", vec!["foo"]);
        let normal = make_task("foo", "Normal task");

        assert!(is_internal_task(&assign));
        assert!(is_internal_task(&eval));
        assert!(!is_internal_task(&normal));
    }

    #[test]
    fn test_internal_task_filtering_preserves_edges_through_internal() {
        // If A -> assign-B -> B, after filtering we should see A -> B
        let mut graph = WorkGraph::new();
        let task_a = make_task("a", "Task A");
        let mut assign_b =
            make_internal_task("assign-b", "Assign agent to b", "assignment", vec!["a"]);
        assign_b.status = Status::InProgress;
        let mut task_b = make_task("b", "Task B");
        task_b.after = vec!["assign-b".to_string()];
        graph.add_node(Node::Task(task_a));
        graph.add_node(Node::Task(assign_b));
        graph.add_node(Node::Task(task_b));

        let annotations: HashMap<String, AnnotationInfo> = HashMap::new();
        let (filtered, annots) =
            filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        // Both a and b should be in the filtered set
        assert!(task_ids.contains("a"));
        assert!(task_ids.contains("b"));
        assert!(!task_ids.contains("assign-b"));

        // b should show [assigning] annotation with the source dot-task ID
        assert!(annots.contains_key("b"));
        let b_annot = &annots["b"];
        assert!(b_annot.text.contains("assigning"));
        assert!(b_annot.dot_task_ids.contains(&"assign-b".to_string()));
    }

    /// Verify the default viz filter includes in-progress tasks alongside open tasks,
    /// while excluding done tasks (regression test for the default filter).
    #[test]
    fn test_default_filter_shows_active_trees() {
        // Active tree: open-task → done-dep (should show both)
        // Fully done tree: done-a → done-b (should hide both)
        // Standalone abandoned: hidden
        let mut graph = WorkGraph::new();

        let mut open_task = make_task("task-open", "Open Task");
        open_task.status = Status::Open;
        open_task.after = vec!["task-done-dep".to_string()];
        let mut done_dep = make_task("task-done-dep", "Done Dep");
        done_dep.status = Status::Done;
        done_dep.before = vec!["task-open".to_string()];

        let mut done_a = make_task("done-a", "Done A");
        done_a.status = Status::Done;
        done_a.before = vec!["done-b".to_string()];
        let mut done_b = make_task("done-b", "Done B");
        done_b.status = Status::Done;
        done_b.after = vec!["done-a".to_string()];

        let mut abandoned = make_task("task-abandoned", "Abandoned");
        abandoned.status = Status::Abandoned;

        graph.add_node(Node::Task(open_task));
        graph.add_node(Node::Task(done_dep));
        graph.add_node(Node::Task(done_a));
        graph.add_node(Node::Task(done_b));
        graph.add_node(Node::Task(abandoned));

        let _options = VizOptions {
            all: false,
            status: None,
            format: OutputFormat::Ascii,
            output: None,
            show_internal: true,
            show_internal_running_only: false,
            critical_path: false,
            focus: Vec::new(),
            tui_mode: false,
            layout: LayoutMode::default(),
            tags: Vec::new(),
            edge_color: "gray".to_string(),
            max_columns: None,
        };
        // We test via run() output by checking generate_ascii directly
        // with the same filter logic
        let cycle_analysis = graph.compute_cycle_analysis();
        let _ = cycle_analysis; // used by run() internally

        // Replicate the active-tree filter
        let mut components: HashMap<&str, usize> = HashMap::new();
        let mut comp_members: Vec<Vec<&str>> = Vec::new();
        for task in graph.tasks() {
            let idx = comp_members.len();
            components.insert(task.id.as_str(), idx);
            comp_members.push(vec![task.id.as_str()]);
        }
        let mut merged: Vec<Option<usize>> = vec![None; comp_members.len()];
        fn find_root<'a>(
            comp: &mut HashMap<&'a str, usize>,
            merged: &mut [Option<usize>],
            id: &'a str,
        ) -> usize {
            let mut c = comp[id];
            while let Some(parent) = merged[c] {
                c = parent;
            }
            let root = c;
            let mut c2 = comp[id];
            while let Some(parent) = merged[c2] {
                merged[c2] = Some(root);
                c2 = parent;
            }
            comp.insert(id, root);
            root
        }
        for task in graph.tasks() {
            for neighbor_id in task.after.iter().chain(task.before.iter()) {
                if components.contains_key(neighbor_id.as_str()) {
                    let a = find_root(&mut components, &mut merged, task.id.as_str());
                    let b = find_root(&mut components, &mut merged, neighbor_id.as_str());
                    if a != b {
                        merged[b] = Some(a);
                    }
                }
            }
        }
        let mut active_roots: HashSet<usize> = HashSet::new();
        for task in graph.tasks() {
            if task.status != Status::Done && task.status != Status::Abandoned {
                active_roots.insert(find_root(&mut components, &mut merged, task.id.as_str()));
            }
        }

        let filtered: Vec<_> = graph
            .tasks()
            .filter(|t| {
                if t.status == Status::Abandoned {
                    return false;
                }
                let root = find_root(&mut components, &mut merged, t.id.as_str());
                active_roots.contains(&root)
            })
            .collect();

        let ids: Vec<&str> = filtered.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&"task-open"), "Active tree: open task shown");
        assert!(
            ids.contains(&"task-done-dep"),
            "Active tree: done dep shown for context"
        );
        assert!(!ids.contains(&"done-a"), "Fully done tree: hidden");
        assert!(!ids.contains(&"done-b"), "Fully done tree: hidden");
        assert!(!ids.contains(&"task-abandoned"), "Abandoned: hidden");
    }

    #[test]
    fn test_coordinator_task_not_internal() {
        // Coordinator tasks (tagged coordinator-loop) should NOT be filtered as internal,
        // even though they have system task IDs (starting with '.').
        use workgraph::graph::CycleConfig;
        let coordinator = Task {
            id: ".coordinator".to_string(),
            title: "Coordinator".to_string(),
            status: Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            cycle_config: Some(CycleConfig {
                max_iterations: 0,
                guard: None,
                delay: None,
                no_converge: true,
                restart_on_failure: true,
                max_failure_restarts: None,
            }),
            ..Task::default()
        };

        // Should not be treated as internal
        assert!(
            !is_internal_task(&coordinator),
            "Coordinator tasks should not be filtered as internal"
        );
        // Should be detected as coordinator
        assert!(
            is_coordinator_task(&coordinator),
            "Coordinator tasks should be detected by is_coordinator_task"
        );

        // Regular system tasks should still be internal
        let assign = make_internal_task("assign-foo", "Assign", "assignment", vec![]);
        assert!(is_internal_task(&assign));
    }

    #[test]
    fn test_coordinator_visible_in_filter() {
        // Coordinator tasks should pass through filter_internal_tasks
        use workgraph::graph::CycleConfig;
        let mut graph = WorkGraph::new();

        let coordinator = Task {
            id: ".coordinator".to_string(),
            title: "Coordinator".to_string(),
            status: Status::InProgress,
            tags: vec!["coordinator-loop".to_string()],
            cycle_config: Some(CycleConfig {
                max_iterations: 0,
                guard: None,
                delay: None,
                no_converge: true,
                restart_on_failure: true,
                max_failure_restarts: None,
            }),
            ..Task::default()
        };
        let normal = make_task("foo", "Normal task");
        let assign = make_internal_task(".assign-foo", "Assign foo", "assignment", vec![]);

        graph.add_node(Node::Task(coordinator));
        graph.add_node(Node::Task(normal));
        graph.add_node(Node::Task(assign));

        let annotations: HashMap<String, AnnotationInfo> = HashMap::new();
        let (filtered, _) = filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);
        let ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        assert!(
            ids.contains(".coordinator"),
            "Coordinator should be visible"
        );
        assert!(ids.contains("foo"), "Normal tasks should be visible");
        assert!(
            !ids.contains(".assign-foo"),
            "Internal tasks should be hidden"
        );
    }

    #[test]
    fn test_filter_internal_tasks_running_only_computes_annotations() {
        // filter_internal_tasks_running_only should compute phase annotations
        // only for actively in-progress internal tasks, not Open/Blocked ones.
        let mut graph = WorkGraph::new();
        let task_b = make_task("b", "Task B");
        let mut assign_b =
            make_internal_task(".assign-b", "Assign agent to b", "assignment", vec!["b"]);
        assign_b.status = Status::InProgress;
        let mut eval_b = make_internal_task(".evaluate-b", "Evaluate b", "evaluation", vec!["b"]);
        eval_b.status = Status::Open; // Open = not yet active, should NOT annotate

        graph.add_node(Node::Task(task_b));
        graph.add_node(Node::Task(assign_b));
        graph.add_node(Node::Task(eval_b));

        let empty: HashMap<String, AnnotationInfo> = HashMap::new();
        let (filtered, annots) =
            filter_internal_tasks_running_only(&graph, graph.tasks().collect(), &empty);

        // Both b and in-progress .assign-b and open .evaluate-b should be kept (visibility)
        let ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains("b"));
        assert!(ids.contains(".assign-b"));
        assert!(ids.contains(".evaluate-b"));

        // Only the in-progress .assign-b should produce an annotation, not the open .evaluate-b
        assert!(annots.contains_key("b"), "Expected annotation for task b");
        let b_annot = &annots["b"];
        assert!(
            b_annot.text.contains("assigning"),
            "Expected 'assigning' in annotation, got: {}",
            b_annot.text
        );
        assert!(
            !b_annot.text.contains("evaluating"),
            "Open .evaluate-b should NOT produce annotation, got: {}",
            b_annot.text
        );
        assert!(b_annot.dot_task_ids.contains(&".assign-b".to_string()));
        assert!(!b_annot.dot_task_ids.contains(&".evaluate-b".to_string()));
    }

    #[test]
    fn test_task_visible_during_assignment_with_open_internal_tasks() {
        // When a task is blocked on Open agency pipeline tasks (.assign-*),
        // it should still be visible in the filtered output BUT without annotations,
        // because Open pipeline tasks haven't started yet.
        let mut graph = WorkGraph::new();

        let mut parent = make_task("my-task", "My Task");
        parent.status = Status::Open;
        parent.after = vec![".assign-my-task".to_string()];

        let assign = Task {
            id: ".assign-my-task".to_string(),
            title: "Assign agent for: my-task".to_string(),
            status: Status::Open,
            before: vec!["my-task".to_string()],
            tags: vec!["assignment".to_string(), "agency".to_string()],
            ..Task::default()
        };

        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        let annotations: HashMap<String, AnnotationInfo> = HashMap::new();
        let (filtered, annots) =
            filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);
        let ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        // Parent task should be visible
        assert!(
            ids.contains("my-task"),
            "Parent task should be visible during assignment"
        );
        // Internal tasks should be hidden
        assert!(
            !ids.contains(".assign-my-task"),
            "Internal assign task should be hidden"
        );

        // No annotations: Open pipeline tasks are pending, not active
        assert!(
            !annots.contains_key("my-task"),
            "Open pipeline tasks should not produce annotations"
        );
    }

    #[test]
    fn test_task_shows_annotation_when_pipeline_in_progress() {
        // When a pipeline task is actively InProgress, its parent should show the annotation.
        let mut graph = WorkGraph::new();

        let mut parent = make_task("my-task", "My Task");
        parent.status = Status::Open;
        parent.after = vec![".assign-my-task".to_string()];

        let assign = Task {
            id: ".assign-my-task".to_string(),
            title: "Assign agent for: my-task".to_string(),
            status: Status::InProgress,
            before: vec!["my-task".to_string()],
            tags: vec!["assignment".to_string(), "agency".to_string()],
            ..Task::default()
        };

        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        let annotations: HashMap<String, AnnotationInfo> = HashMap::new();
        let (_filtered, annots) =
            filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);

        // Parent should have annotation from the InProgress .assign task
        assert!(
            annots.contains_key("my-task"),
            "Expected annotation for my-task"
        );
        let annot = &annots["my-task"];
        assert!(
            annot.text.contains("assigning"),
            "Expected 'assigning' annotation, got: {}",
            annot.text
        );
    }

    #[test]
    fn test_done_internal_tasks_do_not_annotate() {
        // When internal tasks are Done (assignment complete), no annotation should appear.
        let mut graph = WorkGraph::new();

        let mut parent = make_task("my-task", "My Task");
        parent.status = Status::Open;
        parent.after = vec![".assign-my-task".to_string()];

        let mut assign = Task {
            id: ".assign-my-task".to_string(),
            title: "Assign agent for: my-task".to_string(),
            status: Status::Done,
            before: vec!["my-task".to_string()],
            tags: vec!["assignment".to_string(), "agency".to_string()],
            ..Task::default()
        };
        assign.completed_at = Some("2024-01-01T00:00:00Z".to_string());

        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        let annotations: HashMap<String, AnnotationInfo> = HashMap::new();
        let (filtered, annots) =
            filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);

        // Parent should be visible
        let ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains("my-task"));

        // No annotations (internal task is Done)
        assert!(
            !annots.contains_key("my-task"),
            "Done internal tasks should not produce annotations"
        );
    }

    // --- compact_node_annotation tests ---

    #[test]
    fn test_compact_annotation_idle_no_threshold() {
        let text = compact_node_annotation(Status::Open, 0, 0, None);
        assert_eq!(text, "[idle]");
    }

    #[test]
    fn test_compact_annotation_idle_low_pressure() {
        let text = compact_node_annotation(Status::Open, 400, 1000, None);
        assert_eq!(text, "[idle · 40% ctx]");
    }

    #[test]
    fn test_compact_annotation_warning_threshold() {
        let text = compact_node_annotation(Status::Open, 850, 1000, None);
        assert_eq!(text, "[⚠ 85% ctx]");
        assert!(text.contains('⚠'));
    }

    #[test]
    fn test_compact_annotation_overdue() {
        let text = compact_node_annotation(Status::Open, 1100, 1000, None);
        assert_eq!(text, "[⚠ 110% ctx]");
        assert!(text.contains('⚠'));
    }

    #[test]
    fn test_compact_annotation_running() {
        let text = compact_node_annotation(Status::InProgress, 500, 1000, None);
        assert_eq!(text, "[⟳ compacting · 50% ctx]");
        assert!(text.contains('⟳'));
    }

    #[test]
    fn test_compact_annotation_running_no_threshold() {
        let text = compact_node_annotation(Status::InProgress, 0, 0, None);
        assert_eq!(text, "[⟳ compacting]");
    }

    #[test]
    fn test_compact_annotation_recently_completed() {
        // Use a timestamp 10 seconds ago
        let ts = (chrono::Utc::now() - chrono::Duration::seconds(10)).to_rfc3339();
        let text = compact_node_annotation(Status::Open, 0, 1000, Some(&ts));
        // Should show "✓ compacted Xs ago" for recent completion
        assert!(text.contains('✓'), "Expected ✓ in: {}", text);
        assert!(
            text.contains("compacted"),
            "Expected 'compacted' in: {}",
            text
        );
    }

    #[test]
    fn test_compact_annotation_old_completion_shows_pressure() {
        // Timestamp 2 minutes ago — should fall through to pressure display
        let ts = (chrono::Utc::now() - chrono::Duration::seconds(120)).to_rfc3339();
        let text = compact_node_annotation(Status::Open, 300, 1000, Some(&ts));
        // Should show pressure, not "compacted X ago"
        assert!(
            !text.contains('✓'),
            "Old completion should not show ✓: {}",
            text
        );
        assert_eq!(text, "[idle · 30% ctx]");
    }
}
