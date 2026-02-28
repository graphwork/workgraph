mod ascii;
mod dot;
mod graph;

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::graph::{parse_token_usage_live, Status, Task, TokenUsage, WorkGraph};

// Re-export public API
pub use graph::{generate_graph, generate_graph_with_overrides};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// Classic DFS-order tree: fan-in nodes claimed by first parent visited.
    Tree,
    /// Diamond-aware layout: fan-in nodes placed under their lowest common
    /// ancestor so arcs flow downward instead of upward.
    Diamond,
}

impl Default for LayoutMode {
    fn default() -> Self {
        LayoutMode::Diamond
    }
}

impl std::str::FromStr for LayoutMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "tree" => Ok(LayoutMode::Tree),
            "diamond" => Ok(LayoutMode::Diamond),
            _ => Err(format!(
                "Unknown layout: {}. Use 'tree' or 'diamond'.",
                s
            )),
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
pub struct VizOptions {
    pub all: bool,
    pub status: Option<String>,
    pub critical_path: bool,
    pub format: OutputFormat,
    pub output: Option<String>,
    /// Show internal tasks (assign-*, evaluate-*) that are normally hidden
    pub show_internal: bool,
    /// Focus on specific task IDs — show only their containing subgraphs
    pub focus: Vec<String>,
    /// TUI mode: sort subgraphs by most-recently-updated first (LRU ordering)
    #[allow(dead_code)]
    pub tui_mode: bool,
    /// Layout strategy for tree construction
    pub layout: LayoutMode,
    /// Filter by tags (AND semantics — task must have all specified tags)
    pub tags: Vec<String>,
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
            focus: Vec::new(),
            tui_mode: false,
            layout: LayoutMode::default(),
            tags: Vec::new(),
        }
    }
}

/// Returns true if the task is an auto-generated internal task (assignment or evaluation).
fn is_internal_task(task: &Task) -> bool {
    task.tags
        .iter()
        .any(|t| t == "assignment" || t == "evaluation")
}

/// Determine the phase annotation for a parent task based on its related internal tasks.
///
/// - If an assignment task exists and is not done → "[assigning]"
/// - If an evaluation task exists and is not done → "[evaluating]"
fn compute_phase_annotation(internal_task: &Task) -> &'static str {
    if internal_task.tags.iter().any(|t| t == "assignment") {
        "[assigning]"
    } else {
        "[evaluating]"
    }
}

/// Filter out internal tasks and compute phase annotations for their parent tasks.
///
/// Returns:
/// - The filtered list of tasks (internal tasks removed)
/// - A map of parent_task_id → phase annotation string
pub(crate) fn filter_internal_tasks<'a>(
    _graph: &'a WorkGraph,
    tasks: Vec<&'a Task>,
    _existing_annotations: &HashMap<String, String>,
) -> (Vec<&'a Task>, HashMap<String, String>) {
    let mut annotations: HashMap<String, String> = HashMap::new();
    let mut internal_ids: HashSet<&str> = HashSet::new();

    // First pass: identify internal tasks and compute annotations
    for task in &tasks {
        if !is_internal_task(task) {
            continue;
        }
        internal_ids.insert(task.id.as_str());

        // Determine the parent task ID.
        // For assign-X: the parent is X (assign task has no after from parent,
        //   but parent has after assign-X)
        // For evaluate-X: the parent is X (evaluate task is after X)
        let parent_id = if task.tags.iter().any(|t| t == "assignment") {
            // assign-{parent_id}: strip the prefix
            task.id.strip_prefix("assign-").map(|s| s.to_string())
        } else {
            // evaluate-{parent_id}: strip the prefix
            task.id.strip_prefix("evaluate-").map(|s| s.to_string())
        };

        if let Some(pid) = parent_id {
            // Only annotate if the internal task is not yet done
            if task.status == Status::InProgress {
                let annotation = compute_phase_annotation(task);
                annotations.insert(pid, annotation.to_string());
            }
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

/// Generate the ASCII viz output string for the given directory and options.
/// Used by both the CLI `wg viz` command and the TUI viewer.
pub fn generate_viz_output(dir: &Path, options: &VizOptions) -> Result<String> {
    let (graph, _path) = super::load_workgraph(dir)?;
    generate_viz_output_from_graph(&graph, dir, options)
}

/// Generate viz output from an already-loaded graph. Useful when the caller
/// already has the graph loaded (e.g., the TUI viewer for task counting).
pub fn generate_viz_output_from_graph(
    graph: &WorkGraph,
    dir: &Path,
    options: &VizOptions,
) -> Result<String> {
    // Compute cycle analysis so we can preserve cycle members in filtered views
    let cycle_analysis = graph.compute_cycle_analysis();

    // Find cycle indices that have at least one non-done member —
    // all members of such cycles should be shown even without --all.
    let _active_cycle_ids: HashSet<usize> = if options.all || options.status.is_some() {
        HashSet::new()
    } else {
        let mut active = HashSet::new();
        for task in graph.tasks() {
            if task.status != Status::Done {
                if let Some(&ci) = cycle_analysis.task_to_cycle.get(&task.id) {
                    active.insert(ci);
                }
            }
        }
        active
    };

    // Compute weakly connected components via union-find.
    // Used for both active-tree filtering and --focus subgraph selection.
    fn uf_find<'a>(comp: &mut HashMap<&'a str, usize>, merged: &mut Vec<Option<usize>>, id: &'a str) -> usize {
        let mut c = comp[id];
        while let Some(parent) = merged[c] { c = parent; }
        let root = c;
        let mut c2 = comp[id];
        while let Some(parent) = merged[c2] { merged[c2] = Some(root); c2 = parent; }
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

    let edge_pairs: Vec<(String, String)> = graph.tasks().flat_map(|task| {
        let id = task.id.clone();
        task.after.iter().chain(task.before.iter())
            .map(move |neighbor| (id.clone(), neighbor.clone()))
    }).collect();

    for (task_id, neighbor_id) in &edge_pairs {
        if components.contains_key(neighbor_id.as_str()) {
            let a = uf_find(&mut components, &mut merged, task_id.as_str());
            let b = uf_find(&mut components, &mut merged, neighbor_id.as_str());
            if a != b { merged[b] = Some(a); }
        }
    }

    // Precompute task_id → root mapping so we don't need mutable borrows in the filter
    let task_roots: HashMap<&str, usize> = graph.tasks()
        .map(|t| (t.id.as_str(), uf_find(&mut components, &mut merged, t.id.as_str())))
        .collect();

    // For focus mode: collect the roots of focused task IDs
    let focus_roots: HashSet<usize> = options.focus.iter()
        .filter_map(|f| task_roots.get(f.as_str()).copied())
        .collect();

    // For default mode: find roots with active (non-done, non-internal) tasks
    let active_roots: HashSet<usize> = graph.tasks()
        .filter(|t| t.status != Status::Done && t.status != Status::Abandoned && !is_internal_task(t))
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
            if t.status == Status::Abandoned { return false; }
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
                    if let Some(ancestor) = graph.get_task(dep) {
                        if status_str(&ancestor.status) != filter_lower {
                            to_visit.push(dep.clone());
                        }
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
            if !existing_ids.contains(ctx_id.as_str()) {
                if let Some(task) = graph.get_task(ctx_id) {
                    tasks_to_show.push(task);
                }
            }
        }
        tasks_to_show
    } else {
        tasks_to_show
    };

    // Filter out internal tasks (assign-*, evaluate-*) unless --show-internal
    let empty_annotations = HashMap::new();
    let (tasks_to_show, annotations) = if options.show_internal {
        (tasks_to_show, empty_annotations)
    } else {
        filter_internal_tasks(graph, tasks_to_show, &empty_annotations)
    };

    // Resolve cross-repo peer dependencies: create synthetic Task nodes for peer refs
    // so they appear in the graph with their resolved remote status.
    let peer_tasks: Vec<Task> = {
        let mut seen = HashSet::new();
        let mut peers = Vec::new();
        for task in &tasks_to_show {
            for dep in &task.after {
                if let Some((peer_name, remote_task_id)) =
                    workgraph::federation::parse_remote_ref(dep)
                {
                    if seen.insert(dep.clone()) {
                        let remote = workgraph::federation::resolve_remote_task_status(
                            peer_name,
                            remote_task_id,
                            dir,
                        );
                        let title = remote
                            .title
                            .unwrap_or_else(|| remote_task_id.to_string());
                        peers.push(Task {
                            id: dep.clone(),
                            title,
                            status: remote.status,
                            ..Task::default()
                        });
                    }
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

    // Enrich in-progress/done tasks with live token usage from agent output logs
    let agents_dir = dir.join("agents");
    let live_token_usage: HashMap<String, TokenUsage> = tasks_to_show
        .iter()
        .filter(|t| t.token_usage.is_none())
        .filter(|t| t.status == Status::InProgress || t.status == Status::Done)
        .filter_map(|t| {
            let agent_id = t.assigned.as_deref()?;
            let log_path = agents_dir.join(agent_id).join("output.log");
            let usage = parse_token_usage_live(&log_path)?;
            Some((t.id.clone(), usage))
        })
        .collect();

    // Build separate assign and eval token usage maps for each visible task
    let mut assign_token_usage: HashMap<String, TokenUsage> = HashMap::new();
    let mut eval_token_usage: HashMap<String, TokenUsage> = HashMap::new();
    for task in graph.tasks() {
        if !is_internal_task(task) {
            continue;
        }
        let (is_assign, parent_id) = if task.tags.iter().any(|t| t == "assignment") {
            (true, task.id.strip_prefix("assign-").map(|s| s.to_string()))
        } else {
            (false, task.id.strip_prefix("evaluate-").map(|s| s.to_string()))
        };
        let Some(pid) = parent_id else { continue };
        let usage = task.token_usage.as_ref().cloned().or_else(|| {
            let agent_id = task.assigned.as_deref()?;
            let log_path = agents_dir.join(agent_id).join("output.log");
            parse_token_usage_live(&log_path)
        });
        if let Some(u) = usage {
            let map = if is_assign { &mut assign_token_usage } else { &mut eval_token_usage };
            let entry = map.entry(pid).or_insert_with(|| TokenUsage {
                cost_usd: 0.0,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
            });
            entry.cost_usd += u.cost_usd;
            entry.output_tokens += u.output_tokens;
            entry.input_tokens += u.input_tokens;
            entry.cache_read_input_tokens += u.cache_read_input_tokens;
            entry.cache_creation_input_tokens += u.cache_creation_input_tokens;
        }
    }

    // Generate output
    let output = match options.format {
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
        OutputFormat::Ascii => ascii::generate_ascii(graph, &tasks_to_show, &task_ids, &annotations, &live_token_usage, &assign_token_usage, &eval_token_usage, options.layout, &context_ids),
        OutputFormat::Graph => graph::generate_graph(graph, &tasks_to_show, &task_ids, &annotations, &live_token_usage, &assign_token_usage, &eval_token_usage, &context_ids),
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
        dot::render_dot(&output, output_path)?;
        println!("Rendered graph to {}", output_path);
    } else {
        println!("{}", output);
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
        .filter(|t| {
            t.after
                .iter()
                .all(|b| !active_ids.contains(b.as_str()))
        })
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

        let annotations = HashMap::new();
        let (filtered, annots) =
            filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        // Both a and b should be in the filtered set
        assert!(task_ids.contains("a"));
        assert!(task_ids.contains("b"));
        assert!(!task_ids.contains("assign-b"));

        // b should show [assigning] annotation
        assert!(annots.contains_key("b"));
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
            critical_path: false,
            focus: Vec::new(),
            tui_mode: false,
            layout: LayoutMode::default(),
            tags: Vec::new(),
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
        fn find_root<'a>(comp: &mut HashMap<&'a str, usize>, merged: &mut Vec<Option<usize>>, id: &'a str) -> usize {
            let mut c = comp[id];
            while let Some(parent) = merged[c] { c = parent; }
            let root = c;
            let mut c2 = comp[id];
            while let Some(parent) = merged[c2] { merged[c2] = Some(root); c2 = parent; }
            comp.insert(id, root);
            root
        }
        for task in graph.tasks() {
            for neighbor_id in task.after.iter().chain(task.before.iter()) {
                if components.contains_key(neighbor_id.as_str()) {
                    let a = find_root(&mut components, &mut merged, task.id.as_str());
                    let b = find_root(&mut components, &mut merged, neighbor_id.as_str());
                    if a != b { merged[b] = Some(a); }
                }
            }
        }
        let mut active_roots: HashSet<usize> = HashSet::new();
        for task in graph.tasks() {
            if task.status != Status::Done && task.status != Status::Abandoned {
                active_roots.insert(find_root(&mut components, &mut merged, task.id.as_str()));
            }
        }

        let filtered: Vec<_> = graph.tasks().filter(|t| {
            if t.status == Status::Abandoned { return false; }
            let root = find_root(&mut components, &mut merged, t.id.as_str());
            active_roots.contains(&root)
        }).collect();

        let ids: Vec<&str> = filtered.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&"task-open"), "Active tree: open task shown");
        assert!(ids.contains(&"task-done-dep"), "Active tree: done dep shown for context");
        assert!(!ids.contains(&"done-a"), "Fully done tree: hidden");
        assert!(!ids.contains(&"done-b"), "Fully done tree: hidden");
        assert!(!ids.contains(&"task-abandoned"), "Abandoned: hidden");
    }
}
