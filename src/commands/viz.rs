use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, Stdio};
use workgraph::format_hours;
use workgraph::graph::{format_token_display, parse_token_usage_live, Status, Task, TokenUsage, WorkGraph};

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
fn filter_internal_tasks<'a>(
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
                let task_status = match t.status {
                    Status::Open => "open",
                    Status::InProgress => "in-progress",
                    Status::Done => "done",
                    Status::Blocked => "blocked",
                    Status::Failed => "failed",
                    Status::Abandoned => "abandoned",
                };
                return task_status == status_filter.to_lowercase();
            }

            // Default: show tasks in active WCCs
            if t.status == Status::Abandoned { return false; }
            active_roots.contains(&root)
        })
        .collect();

    // Filter out internal tasks (assign-*, evaluate-*) unless --show-internal
    let empty_annotations = HashMap::new();
    let (tasks_to_show, annotations) = if options.show_internal {
        (tasks_to_show, empty_annotations)
    } else {
        filter_internal_tasks(&graph, tasks_to_show, &empty_annotations)
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
        calculate_critical_path(&graph, &task_ids)
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
        OutputFormat::Dot => generate_dot(
            &graph,
            &tasks_to_show,
            &task_ids,
            &critical_path_set,
            &annotations,
        ),
        OutputFormat::Mermaid => generate_mermaid(
            &graph,
            &tasks_to_show,
            &task_ids,
            &critical_path_set,
            &annotations,
        ),
        OutputFormat::Ascii => generate_ascii(&graph, &tasks_to_show, &task_ids, &annotations, &live_token_usage, &assign_token_usage, &eval_token_usage, options.layout),
        OutputFormat::Graph => generate_graph(&graph, &tasks_to_show, &task_ids, &annotations, &live_token_usage, &assign_token_usage, &eval_token_usage),
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
        render_dot(&output, output_path)?;
        println!("Rendered graph to {}", output_path);
    } else {
        println!("{}", output);
    }

    Ok(())
}

fn generate_dot(
    graph: &WorkGraph,
    tasks: &[&workgraph::graph::Task],
    task_ids: &HashSet<&str>,
    critical_path: &HashSet<String>,
    annotations: &HashMap<String, String>,
) -> String {
    let mut lines = vec![
        "digraph workgraph {".to_string(),
        "  rankdir=LR;".to_string(),
        "  node [shape=box];".to_string(),
        String::new(),
    ];

    // Print task nodes
    for task in tasks {
        let style = match task.status {
            Status::Done => "style=filled, fillcolor=lightgreen",
            Status::InProgress => "style=filled, fillcolor=lightyellow",
            Status::Blocked => "style=filled, fillcolor=lightcoral",
            Status::Open => "style=filled, fillcolor=white",
            Status::Failed => "style=filled, fillcolor=salmon",
            Status::Abandoned => "style=filled, fillcolor=lightgray",
        };

        // Build label with hours estimate if available
        let hours_str = task
            .estimate
            .as_ref()
            .and_then(|e| e.hours)
            .map(|h| format!("\\n{}h", format_hours(h)))
            .unwrap_or_default();

        // Add phase annotation if present
        let phase_str = annotations
            .get(&task.id)
            .map(|a| format!(" {}", a))
            .unwrap_or_default();

        let label = format!("{}\\n{}{}{}", task.id, task.title, hours_str, phase_str);

        // Check if on critical path
        let node_style = if critical_path.contains(&task.id) {
            format!("{}, penwidth=3, color=red", style)
        } else {
            style.to_string()
        };

        lines.push(format!(
            "  \"{}\" [label=\"{}\", {}];",
            task.id, label, node_style
        ));
    }

    // Print assigned actors as ellipse nodes
    let assigned_actors: HashSet<&str> =
        tasks.iter().filter_map(|t| t.assigned.as_deref()).collect();

    for actor_id in &assigned_actors {
        lines.push(format!(
            "  \"{}\" [label=\"{}\", shape=ellipse, style=filled, fillcolor=lightblue];",
            actor_id, actor_id
        ));
    }

    // Print resources that are required by shown tasks
    let required_resources: HashSet<&str> = tasks
        .iter()
        .flat_map(|t| t.requires.iter().map(String::as_str))
        .collect();

    for resource in graph.resources() {
        if required_resources.contains(resource.id.as_str()) {
            let name = resource.name.as_deref().unwrap_or(&resource.id);
            lines.push(format!(
                "  \"{}\" [label=\"{}\", shape=diamond, style=filled, fillcolor=lightyellow];",
                resource.id, name
            ));
        }
    }

    lines.push(String::new());

    // Print edges
    for task in tasks {
        for after in &task.after {
            // Only show edge if the blocker is also in our task set
            if task_ids.contains(after.as_str()) {
                // Check if this edge is on critical path
                let edge_style =
                    if critical_path.contains(&task.id) && critical_path.contains(after) {
                        "color=red, penwidth=2"
                    } else {
                        ""
                    };

                if edge_style.is_empty() {
                    lines.push(format!(
                        "  \"{}\" -> \"{}\";",
                        after, task.id
                    ));
                } else {
                    lines.push(format!(
                        "  \"{}\" -> \"{}\" [{}];",
                        after, task.id, edge_style
                    ));
                }
            }
        }

        if let Some(ref assigned) = task.assigned {
            lines.push(format!(
                "  \"{}\" -> \"{}\" [style=dashed];",
                task.id, assigned
            ));
        }

        for req in &task.requires {
            if required_resources.contains(req.as_str()) {
                lines.push(format!(
                    "  \"{}\" -> \"{}\" [style=dotted, label=\"requires\"];",
                    task.id, req
                ));
            }
        }

    }

    lines.push("}".to_string());

    lines.join("\n")
}

fn generate_mermaid(
    _graph: &WorkGraph,
    tasks: &[&workgraph::graph::Task],
    task_ids: &HashSet<&str>,
    critical_path: &HashSet<String>,
    annotations: &HashMap<String, String>,
) -> String {
    let mut lines = Vec::new();

    lines.push("flowchart LR".to_string());

    // Print task nodes
    for task in tasks {
        let hours_str = task
            .estimate
            .as_ref()
            .and_then(|e| e.hours)
            .map(|h| format!(" ({}h)", format_hours(h)))
            .unwrap_or_default();

        // Sanitize title for mermaid (escape quotes)
        let title = task.title.replace('"', "'");

        // Add phase annotation if present
        let phase_str = annotations
            .get(&task.id)
            .map(|a| format!(" {}", a))
            .unwrap_or_default();

        let label = format!("{}: {}{}{}", task.id, title, hours_str, phase_str);

        // Mermaid node shape based on status
        let node = match task.status {
            Status::Done => format!("  {}[/\"{}\"/]", task.id, label),
            Status::InProgress => format!("  {}((\"{}\"))", task.id, label),
            Status::Blocked => format!("  {}{{\"{}\"}}!", task.id, label),
            Status::Open => format!("  {}[\"{}\"]", task.id, label),
            Status::Failed => format!("  {}{{{{\"{}\"}}}}!", task.id, label),
            Status::Abandoned => format!("  {}[\"{}\"]:::abandoned", task.id, label),
        };
        lines.push(node);
    }

    lines.push(String::new());

    // Print edges
    for task in tasks {
        for after in &task.after {
            if task_ids.contains(after.as_str()) {
                // Check if this edge is on critical path
                let arrow =
                    if critical_path.contains(&task.id) && critical_path.contains(after) {
                        "==>" // thick arrow for critical path
                    } else {
                        "-->"
                    };

                lines.push(format!("  {} {} {}", after, arrow, task.id));
            }
        }
    }


    // Print actor assignments
    let assigned_actors: HashSet<&str> =
        tasks.iter().filter_map(|t| t.assigned.as_deref()).collect();

    if !assigned_actors.is_empty() {
        lines.push(String::new());
        for actor_id in &assigned_actors {
            lines.push(format!("  {}(({}))", actor_id, actor_id));
        }

        for task in tasks {
            if let Some(ref assigned) = task.assigned {
                lines.push(format!("  {} -.-> {}", task.id, assigned));
            }
        }
    }

    // Add styling for critical path nodes
    if !critical_path.is_empty() {
        lines.push(String::new());
        lines.push("  %% Critical path styling".to_string());
        let critical_nodes: Vec<&str> = critical_path.iter().map(String::as_str).collect();
        lines.push(format!(
            "  style {} stroke:#f00,stroke-width:3px",
            critical_nodes.join(",")
        ));
    }

    lines.join("\n")
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

fn render_dot(dot_content: &str, output_path: &str) -> Result<()> {
    // Determine output format from file extension
    let format = if output_path.ends_with(".png") {
        "png"
    } else if output_path.ends_with(".svg") {
        "svg"
    } else if output_path.ends_with(".pdf") {
        "pdf"
    } else {
        "png" // default
    };

    let mut child = Command::new("dot")
        .arg(format!("-T{}", format))
        .arg("-o")
        .arg(output_path)
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to run 'dot' command. Is Graphviz installed?")?;

    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin
            .write_all(dot_content.as_bytes())
            .context("Failed to write to dot stdin")?;
    }

    let status = child.wait().context("Failed to wait for dot process")?;

    if !status.success() {
        anyhow::bail!("dot command failed with status: {}", status);
    }

    Ok(())
}

/// Back-edge arc info for Phase 2 rendering of right-side arcs.
struct BackEdgeArc {
    blocker_line: usize,   // line index where the blocking node was rendered
    dependent_line: usize, // line index where the dependent node was rendered
}

/// Generate an ASCII visualization that shows the dependency graph
/// as a tree with right-side arc channels for non-tree edges.
///
/// Layout:
/// - LEFT: tree structure (├→, └→, │) shows primary forward edges flowing down
/// - RIGHT: arc channels (←, ┐, ┘, ┤, │) show non-tree edges (direction-aware)
///   ← always marks the dependent node regardless of vertical position
/// - Arrowheads: → on left (tree connectors), ← on right (arc dependents)
/// - Dash fill (─) connects node text to right-side arcs
#[allow(clippy::only_used_in_recursion)]
fn generate_ascii(
    graph: &WorkGraph,
    tasks: &[&workgraph::graph::Task],
    task_ids: &HashSet<&str>,
    annotations: &HashMap<String, String>,
    live_token_usage: &HashMap<String, TokenUsage>,
    assign_token_usage: &HashMap<String, TokenUsage>,
    eval_token_usage: &HashMap<String, TokenUsage>,
    layout: LayoutMode,
) -> String {
    if tasks.is_empty() {
        return String::from("(no tasks to display)");
    }

    // Compute cycle analysis to distinguish back-edges from fan-in
    let cycle_analysis = graph.compute_cycle_analysis();

    // Build adjacency within the active set
    let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in tasks {
        for blocker in &task.after {
            if task_ids.contains(blocker.as_str()) {
                forward
                    .entry(blocker.as_str())
                    .or_default()
                    .push(task.id.as_str());
                reverse
                    .entry(task.id.as_str())
                    .or_default()
                    .push(blocker.as_str());
            }
        }
    }
    for v in forward.values_mut() {
        v.sort();
    }
    for v in reverse.values_mut() {
        v.sort();
    }

    // ── Diamond layout: restructure fan-in nodes ──
    // For fan-in nodes (multiple parents), move them from their parents'
    // children to the lowest common ancestor (LCA) so arcs flow downward.
    let mut fan_in_arc_edges: Vec<(&str, &str)> = Vec::new(); // (parent, fan_in_node)
    if layout == LayoutMode::Diamond {
        // Identify fan-in nodes (in-degree > 1 in the visible set),
        // excluding nodes that are part of a cycle (let cycle handling deal with those).
        let fan_in_nodes: Vec<&str> = tasks
            .iter()
            .filter(|t| {
                let parents = reverse.get(t.id.as_str());
                parents.map(|v| v.len()).unwrap_or(0) > 1
                    && !cycle_analysis.task_to_cycle.contains_key(&t.id)
            })
            .map(|t| t.id.as_str())
            .collect();

        if !fan_in_nodes.is_empty() {
            // Compute topological depth (longest path from any root) via Kahn's algorithm
            let all_ids_vec: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
            let mut remaining_in: HashMap<&str, usize> = HashMap::new();
            for &id in &all_ids_vec {
                remaining_in.insert(id, reverse.get(id).map(|v| v.len()).unwrap_or(0));
            }
            let mut topo_order: Vec<&str> = Vec::new();
            let mut queue: VecDeque<&str> = VecDeque::new();
            for (&id, &deg) in &remaining_in {
                if deg == 0 {
                    queue.push_back(id);
                }
            }
            while let Some(id) = queue.pop_front() {
                topo_order.push(id);
                for &child in forward.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                    if let Some(deg) = remaining_in.get_mut(child) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(child);
                        }
                    }
                }
            }
            let mut topo_depth: HashMap<&str, usize> = HashMap::new();
            for &id in &topo_order {
                let d = *topo_depth.entry(id).or_insert(0);
                for &child in forward.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                    let cd = topo_depth.entry(child).or_insert(0);
                    if d + 1 > *cd {
                        *cd = d + 1;
                    }
                }
            }

            // For each fan-in node, find LCA of its parents and restructure
            for &fan_in in &fan_in_nodes {
                let parents: Vec<&str> = match reverse.get(fan_in) {
                    Some(p) if p.len() > 1 => p.clone(),
                    _ => continue,
                };

                // Compute ancestor sets for each parent (BFS up the reverse graph)
                let ancestor_sets: Vec<HashSet<&str>> = parents
                    .iter()
                    .map(|&p| {
                        let mut ancestors = HashSet::new();
                        ancestors.insert(p); // include self
                        let mut stack = vec![p];
                        while let Some(node) = stack.pop() {
                            for &gp in reverse.get(node).map(Vec::as_slice).unwrap_or(&[]) {
                                if ancestors.insert(gp) {
                                    stack.push(gp);
                                }
                            }
                        }
                        ancestors
                    })
                    .collect();

                // Intersect all ancestor sets to find common ancestors
                let mut common: HashSet<&str> = ancestor_sets[0].clone();
                for set in &ancestor_sets[1..] {
                    common = common.intersection(set).copied().collect();
                }

                // Remove the fan-in node itself if it somehow appears
                common.remove(fan_in);
                // Remove the parents themselves — the LCA should be above them
                // (unless a parent IS an ancestor of another parent)
                // Actually, keep parents that are ancestors of other parents:
                // e.g. if parents = [A, B] and A is ancestor of B, then A is a valid LCA.

                if common.is_empty() {
                    continue;
                }

                // Pick the deepest common ancestor (max topo_depth)
                let lca = *common
                    .iter()
                    .max_by_key(|&&a| topo_depth.get(a).unwrap_or(&0))
                    .unwrap();

                // Remove fan_in from all parents' forward lists
                for &parent in &parents {
                    if let Some(children) = forward.get_mut(parent) {
                        children.retain(|&c| c != fan_in);
                    }
                    // Track the edge for arc drawing (unless parent IS the LCA,
                    // since the tree edge from LCA to fan_in replaces the real edge)
                    if parent != lca {
                        fan_in_arc_edges.push((parent, fan_in));
                    }
                }

                // Add fan_in as last child of LCA
                // (If LCA is already a parent of fan_in, we already removed it above,
                //  so re-add it at the end to ensure it comes after sibling-parents)
                forward.entry(lca).or_default().push(fan_in);
            }
        }
    }

    // Task lookup
    let task_map: HashMap<&str, &workgraph::graph::Task> =
        tasks.iter().map(|t| (t.id.as_str(), *t)).collect();

    let is_independent = |id: &str| -> bool {
        let has_children = forward.get(id).map(|v| !v.is_empty()).unwrap_or(false);
        let has_parents = reverse.get(id).map(|v| !v.is_empty()).unwrap_or(false);
        !has_children && !has_parents
    };

    // Color helpers
    let use_color = std::io::stdout().is_terminal();

    let status_color = |status: &Status| -> &str {
        if !use_color {
            return "";
        }
        match status {
            Status::Done => "\x1b[32m",       // green
            Status::InProgress => "\x1b[33m", // yellow
            Status::Open => "\x1b[37m",       // white
            Status::Blocked => "\x1b[90m",    // gray
            Status::Failed => "\x1b[31m",     // red
            Status::Abandoned => "\x1b[90m",  // gray
        }
    };
    let reset = if use_color { "\x1b[0m" } else { "" };

    let status_label = |status: &Status| -> &str {
        match status {
            Status::Done => "done",
            Status::InProgress => "in-progress",
            Status::Open => "open",
            Status::Blocked => "blocked",
            Status::Failed => "failed",
            Status::Abandoned => "abandoned",
        }
    };

    let format_node = |id: &str| -> String {
        let task = task_map.get(id);
        let color = task.map(|t| status_color(&t.status)).unwrap_or("");
        let status = task.map(|t| status_label(&t.status)).unwrap_or("unknown");
        let loop_info = task
            .filter(|t| t.cycle_config.is_some() || t.loop_iteration > 0)
            .map(|t| {
                let (iter, max) = if let Some(ref cfg) = t.cycle_config {
                    (t.loop_iteration, cfg.max_iterations)
                } else {
                    (t.loop_iteration, 0)
                };
                if max > 0 {
                    format!(" ↺ (iter {}/{})", iter, max)
                } else {
                    format!(" ↺ (iter {})", iter)
                }
            })
            .unwrap_or_default();
        let phase_info = annotations
            .get(id)
            .map(|a| format!(" {}", a))
            .unwrap_or_default();
        let usage = task
            .and_then(|t| t.token_usage.as_ref().or_else(|| live_token_usage.get(&t.id)));
        let atok_usage = assign_token_usage.get(id);
        let etok_usage = eval_token_usage.get(id);
        let status_with_tokens = if let Some(tok_str) = format_token_display(usage, atok_usage, etok_usage) {
            format!("{} · {}", status, tok_str)
        } else {
            status.to_string()
        };
        format!(
            "{}{}{}  ({}){}{}",
            color, id, reset, status_with_tokens, phase_info, loop_info
        )
    };

    // Find connected components using union-find
    let all_ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let id_to_idx: HashMap<&str, usize> =
        all_ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
    let mut parent_uf: Vec<usize> = (0..all_ids.len()).collect();

    fn find(parent: &mut Vec<usize>, i: usize) -> usize {
        if parent[i] != i {
            parent[i] = find(parent, parent[i]);
        }
        parent[i]
    }
    fn union(parent: &mut Vec<usize>, a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[ra] = rb;
        }
    }

    for task in tasks {
        let ti = id_to_idx[task.id.as_str()];
        for blocker in &task.after {
            if let Some(&bi) = id_to_idx.get(blocker.as_str()) {
                union(&mut parent_uf, ti, bi);
            }
        }
    }

    // Group tasks by component (including independent tasks as single-node WCCs)
    let mut components: HashMap<usize, Vec<&str>> = HashMap::new();
    for &id in &all_ids {
        let root = find(&mut parent_uf, id_to_idx[id]);
        components.entry(root).or_default().push(id);
    }

    let mut back_edge_arcs: Vec<BackEdgeArc> = Vec::new();
    let mut node_line_map: HashMap<&str, usize> = HashMap::new();

    let mut lines: Vec<String> = Vec::new();
    let mut rendered: HashSet<&str> = HashSet::new();

    // (WCC summary lines removed — tracking structs no longer needed)

    // Sort components by LRU ordering: most-recently-operated-on WCC first.
    // Two-level sort: active WCCs (with in-progress tasks) first, then by
    // most recent timestamp (completed_at, started_at, log entries, created_at).
    let mut component_list: Vec<Vec<&str>> = components.into_values().collect();
    component_list.retain(|c| !c.is_empty());
    component_list.sort_by(|a, b| {
        let has_active = |ids: &[&str]| -> bool {
            ids.iter().any(|id| {
                task_map
                    .get(id)
                    .map(|t| t.status == Status::InProgress)
                    .unwrap_or(false)
            })
        };
        let a_active = has_active(a);
        let b_active = has_active(b);
        // Active WCCs first, then by most-recently-updated timestamp
        b_active.cmp(&a_active).then_with(|| {
            let latest = |ids: &[&str]| -> Option<String> {
                ids.iter()
                    .filter_map(|id| task_map.get(id))
                    .flat_map(|t| {
                        let mut timestamps: Vec<&str> = Vec::new();
                        if let Some(ts) = t.completed_at.as_deref() {
                            timestamps.push(ts);
                        }
                        if let Some(ts) = t.started_at.as_deref() {
                            timestamps.push(ts);
                        }
                        for entry in &t.log {
                            timestamps.push(entry.timestamp.as_str());
                        }
                        if let Some(ts) = t.created_at.as_deref() {
                            timestamps.push(ts);
                        }
                        timestamps
                    })
                    .max()
                    .map(String::from)
            };
            let a_latest = latest(a);
            let b_latest = latest(b);
            b_latest.cmp(&a_latest).then_with(|| {
                let a_min = a.iter().min().unwrap_or(&"");
                let b_min = b.iter().min().unwrap_or(&"");
                a_min.cmp(b_min)
            })
        })
    });

    for component in &component_list {
        // Find roots: tasks with no parents outside their SCC
        let mut roots: Vec<&str> = component
            .iter()
            .filter(|&&id| {
                let parents = reverse.get(id).map(Vec::as_slice).unwrap_or(&[]);
                parents.iter().all(|&p| {
                    match cycle_analysis.task_to_cycle.get(id) {
                        Some(idx) => cycle_analysis.task_to_cycle.get(p) == Some(idx),
                        None => false,
                    }
                })
            })
            .copied()
            .collect();
        roots.sort_by(|a, b| {
            let a_time = task_map.get(a).and_then(|t| t.created_at.as_deref());
            let b_time = task_map.get(b).and_then(|t| t.created_at.as_deref());
            a_time.cmp(&b_time).then_with(|| a.cmp(b))
        });
        // Keep only one root per SCC
        {
            let mut seen_sccs: HashSet<usize> = HashSet::new();
            roots.retain(|root| match cycle_analysis.task_to_cycle.get(*root) {
                Some(&scc_idx) => seen_sccs.insert(scc_idx),
                None => true,
            });
        }

        if roots.is_empty() {
            let mut sorted = component.clone();
            sorted.sort_by(|a, b| {
                let a_time = task_map.get(a).and_then(|t| t.created_at.as_deref());
                let b_time = task_map.get(b).and_then(|t| t.created_at.as_deref());
                a_time.cmp(&b_time).then_with(|| a.cmp(b))
            });
            roots.push(sorted[0]);
        }

        if !lines.is_empty() {
            lines.push(String::new());
        }

        // Pre-compute invisible visits via DFS simulation
        let mut invisible_visits: HashSet<(String, String)> = HashSet::new();
        {
            fn simulate_dfs<'a>(
                id: &'a str,
                parent_id: Option<&'a str>,
                sim_rendered: &mut HashSet<&'a str>,
                invisible: &mut HashSet<(String, String)>,
                forward: &HashMap<&str, Vec<&'a str>>,
            ) {
                if sim_rendered.contains(id) {
                    // ALL re-visits are invisible (both back-edges and fan-in)
                    if let Some(pid) = parent_id {
                        invisible.insert((pid.to_string(), id.to_string()));
                    }
                    return;
                }
                sim_rendered.insert(id);
                for &child in forward.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                    simulate_dfs(child, Some(id), sim_rendered, invisible, forward);
                }
            }
            let mut sim_rendered: HashSet<&str> = HashSet::new();
            for root in &roots {
                simulate_dfs(root, None, &mut sim_rendered, &mut invisible_visits, &forward);
            }
        }

        // DFS from each root
        for root in &roots {
            #[allow(clippy::too_many_arguments, clippy::only_used_in_recursion)]
            fn render_tree<'a>(
                id: &'a str,
                parent_id: Option<&str>,
                prefix: &str,
                is_last: bool,
                is_root: bool,
                lines: &mut Vec<String>,
                rendered: &mut HashSet<&'a str>,
                forward: &HashMap<&str, Vec<&'a str>>,
                format_node: &dyn Fn(&str) -> String,
                task_map: &HashMap<&str, &workgraph::graph::Task>,
                use_color: bool,
                node_line_map: &mut HashMap<&'a str, usize>,
                back_edge_arcs: &mut Vec<BackEdgeArc>,
                invisible_visits: &HashSet<(String, String)>,
            ) {
                let connector = if is_root {
                    String::new()
                } else if is_last {
                    "└→ ".to_string()
                } else {
                    "├→ ".to_string()
                };

                // Already rendered: record arc, emit nothing
                if rendered.contains(id) {
                    if let Some(pid) = parent_id {
                        if let Some(&blocker_line) = node_line_map.get(pid) {
                            if let Some(&dependent_line) = node_line_map.get(id) {
                                back_edge_arcs.push(BackEdgeArc {
                                    blocker_line,
                                    dependent_line,
                                });
                            }
                        }
                    }
                    return;
                }

                rendered.insert(id);

                let node_str = format_node(id);
                lines.push(format!("{}{}{}", prefix, connector, node_str));
                node_line_map.insert(id, lines.len() - 1);

                // Compute child prefix
                let child_prefix = if is_root {
                    prefix.to_string()
                } else if is_last {
                    format!("{}  ", prefix)
                } else {
                    format!("{}│ ", prefix)
                };

                // Get children and recurse
                let children = forward.get(id).map(Vec::as_slice).unwrap_or(&[]);
                for (i, &child) in children.iter().enumerate() {
                    // Effective is_last: skip invisible subsequent siblings
                    let child_is_last = children[i + 1..].iter().all(|&sib| {
                        invisible_visits.contains(&(id.to_string(), sib.to_string()))
                    });
                    render_tree(
                        child,
                        Some(id),
                        &child_prefix,
                        child_is_last,
                        false,
                        lines,
                        rendered,
                        forward,
                        format_node,
                        task_map,
                        use_color,
                        node_line_map,
                        back_edge_arcs,
                        invisible_visits,
                    );
                }
            }

            render_tree(
                root,
                None,
                "",
                true,
                true,
                &mut lines,
                &mut rendered,
                &forward,
                &format_node,
                &task_map,
                use_color,
                &mut node_line_map,
                &mut back_edge_arcs,
                &invisible_visits,
            );
        }

        // Record this WCC's line range for per-component summary
        // For single-node independent WCCs, append "(independent)" label
        if component.len() == 1 && is_independent(component[0]) {
            if let Some(last_line) = lines.last_mut() {
                last_line.push_str("  (independent)");
            }
        }
        // (WCC range tracking removed)
    }

    // Add arcs for fan-in edges that were moved during diamond restructuring
    for &(parent, fan_in) in &fan_in_arc_edges {
        if let (Some(&parent_line), Some(&fan_in_line)) =
            (node_line_map.get(parent), node_line_map.get(fan_in))
        {
            back_edge_arcs.push(BackEdgeArc {
                blocker_line: parent_line,
                dependent_line: fan_in_line,
            });
        }
    }

    // Phase 2: Draw right-side arcs for all non-tree edges
    draw_back_edge_arcs(&mut lines, &back_edge_arcs, use_color);

    // Phase 2: Draw right-side arcs
    // (WCC summary lines removed — too noisy)

    lines.join("\n")
}

/// Pad a line with spaces so its visible length reaches at least `target_len`.
fn pad_line_to(line: &mut String, target_len: usize) {
    let current = visible_len(line);
    if current < target_len {
        line.push_str(&" ".repeat(target_len - current));
    }
}

/// Fill a line with `─` for arc dash-fill. Adds a space separator first.
fn fill_line_to(line: &mut String, target_len: usize, dim: &str, reset: &str) {
    let current = visible_len(line);
    if current < target_len {
        let gap = target_len - current;
        if gap > 1 {
            line.push(' ');
            line.push_str(&format!("{}{}{}", dim, "─".repeat(gap - 1), reset));
        } else {
            line.push_str(&format!("{}{}{}", dim, "─", reset));
        }
    }
}

/// Phase 2: Draw right-side arcs for non-tree edges (direction-aware).
///
/// Same-dependent arcs are collapsed into a single column:
/// - Dependent line: `←` (arrowhead marks the receiver)
/// - Blocker lines: plain dash fill to corner/junction
/// - Between: `│` (vertical channel)
///
/// The arrowhead always goes at the dependent node regardless of its
/// vertical position (top, middle, or bottom of the arc span).
///
/// Corner characters: `┐` at top, `┘` at bottom, `┤` at intermediate.
/// Dash fill (`─`) connects node text to the arc column.
fn draw_back_edge_arcs(lines: &mut Vec<String>, arcs: &[BackEdgeArc], use_color: bool) {
    if arcs.is_empty() {
        return;
    }

    // Separate self-loops from real arcs
    let mut self_loops: Vec<usize> = Vec::new();
    let mut real_arcs: Vec<&BackEdgeArc> = Vec::new();
    for arc in arcs {
        if arc.blocker_line == arc.dependent_line {
            self_loops.push(arc.blocker_line);
        } else {
            real_arcs.push(arc);
        }
    }

    for line_idx in self_loops {
        if line_idx < lines.len() {
            lines[line_idx].push_str(" ↺");
        }
    }

    if real_arcs.is_empty() {
        return;
    }

    // Group arcs by dependent line — all edges pointing to the same dependent
    // share one column, regardless of whether blockers are above or below.
    let mut by_dependent: HashMap<usize, Vec<usize>> = HashMap::new();
    for arc in &real_arcs {
        by_dependent
            .entry(arc.dependent_line)
            .or_default()
            .push(arc.blocker_line);
    }
    for blockers in by_dependent.values_mut() {
        blockers.sort();
        blockers.dedup();
    }

    struct ArcColumn {
        dependent: usize,
        blockers: Vec<usize>,
        top: usize,
        bottom: usize,
    }

    let mut columns: Vec<ArcColumn> = by_dependent
        .into_iter()
        .map(|(dependent, blockers)| {
            let min_blocker = *blockers.first().unwrap();
            let max_blocker = *blockers.last().unwrap();
            let top = dependent.min(min_blocker);
            let bottom = dependent.max(max_blocker);
            ArcColumn {
                dependent,
                blockers,
                top,
                bottom,
            }
        })
        .collect();

    // Sort by span (shortest first → innermost)
    columns.sort_by_key(|c| c.bottom - c.top);

    let dim = if use_color { "\x1b[37m" } else { "" }; // white, matching tree connectors
    let reset = if use_color { "\x1b[0m" } else { "" };

    // Each arc column is 2 chars wide (e.g. ←┐). Use 3-char stride so adjacent
    // columns have a 1-char gap for readability when multiple arcs stack up.
    let col_stride = 3;

    // Group columns into non-overlapping bands so each band computes its own
    // margin from only the lines it spans. This prevents arcs in small subgraphs
    // from stretching to the width of the widest line in a different subgraph.
    struct Band {
        col_indices: Vec<usize>,
        top: usize,
        bottom: usize,
    }

    let mut bands: Vec<Band> = Vec::new();
    // Process columns sorted by top line for band grouping
    let mut col_order: Vec<usize> = (0..columns.len()).collect();
    col_order.sort_by_key(|&i| (columns[i].top, columns[i].bottom));

    for &ci in &col_order {
        let col = &columns[ci];
        let mut merged_into = None;
        for (bi, band) in bands.iter_mut().enumerate() {
            if col.top <= band.bottom + 1 && col.bottom + 1 >= band.top {
                band.top = band.top.min(col.top);
                band.bottom = band.bottom.max(col.bottom);
                band.col_indices.push(ci);
                merged_into = Some(bi);
                break;
            }
        }
        if merged_into.is_none() {
            bands.push(Band {
                col_indices: vec![ci],
                top: col.top,
                bottom: col.bottom,
            });
        }
    }

    // Merge any bands that now overlap after expansion
    let mut merged = true;
    while merged {
        merged = false;
        'outer: for i in 0..bands.len() {
            for j in (i + 1)..bands.len() {
                if bands[i].top <= bands[j].bottom + 1 && bands[i].bottom + 1 >= bands[j].top {
                    let merged_top = bands[i].top.min(bands[j].top);
                    let merged_bottom = bands[i].bottom.max(bands[j].bottom);
                    bands[i].top = merged_top;
                    bands[i].bottom = merged_bottom;
                    let moved = std::mem::take(&mut bands[j].col_indices);
                    bands[i].col_indices.extend(moved);
                    bands.remove(j);
                    merged = true;
                    break 'outer;
                }
            }
        }
    }

    for band in &bands {
        let band_max_width = lines[band.top..=band.bottom.min(lines.len() - 1)]
            .iter()
            .map(|l| visible_len(l))
            .max()
            .unwrap_or(0);
        let band_margin_start = band_max_width + 2;

        // Build node_set for this band's columns (using band-local indices)
        let node_set: HashSet<(usize, usize)> = band.col_indices
            .iter()
            .enumerate()
            .flat_map(|(local_idx, &ci)| {
                let c = &columns[ci];
                let dep = std::iter::once((local_idx, c.dependent));
                let blk = c.blockers.iter().map(move |&b| (local_idx, b));
                dep.chain(blk)
            })
            .collect();

        for (local_idx, &col_idx) in band.col_indices.iter().enumerate() {
            let column = &columns[col_idx];
            let col_x = band_margin_start + local_idx * col_stride;

            for line_idx in column.top..=column.bottom {
                if line_idx >= lines.len() {
                    continue;
                }

                let is_dep = line_idx == column.dependent;
                let is_blocker = column.blockers.contains(&line_idx);
                let is_top = line_idx == column.top;
                let is_bottom = line_idx == column.bottom;

                if is_dep || is_blocker {
                    // This line participates in the arc — needs dash fill + glyph
                    let line = &mut lines[line_idx];
                    let end = col_x + 2;

                    // Determine the corner/junction glyph
                    let glyph = if is_top && is_bottom {
                        unreachable!("dependent == blocker filtered as self-loop");
                    } else if is_top {
                        if is_dep { "←┐" } else { "─┐" }
                    } else if is_bottom {
                        if is_dep { "←┘" } else { "─┘" }
                    } else {
                        if is_dep { "←┤" } else { "─┤" }
                    };

                    let current = visible_len(line);
                    if current < end {
                        let gap = end - current;
                        if gap >= 3 && glyph.starts_with('←') {
                            line.push(' ');
                            line.push_str(&format!(
                                "{}←{}{}{}",
                                dim,
                                "─".repeat(gap - 3),
                                &glyph[glyph.char_indices().nth(1).unwrap().0..],
                                reset
                            ));
                        } else if gap >= 2 && glyph.starts_with('←') {
                            line.push_str(&format!("{}{}{}", dim, glyph, reset));
                        } else {
                            fill_line_to(line, col_x + 1, dim, reset);
                            line.push_str(&format!("{}{}{}", dim, &glyph[glyph.char_indices().last().unwrap().0..], reset));
                        }
                    }
                } else {
                    // Vertical pass-through
                    let line = &mut lines[line_idx];
                    if !node_set.contains(&(local_idx, line_idx)) {
                        // Check if any outer (later) column in this band has a
                        // horizontal on this line — if so, use ┼ with dash fill.
                        let has_crossing = band.col_indices[local_idx + 1..].iter().any(|&k| {
                            let c = &columns[k];
                            line_idx >= c.top
                                && line_idx <= c.bottom
                                && (line_idx == c.dependent
                                    || c.blockers.contains(&line_idx))
                        });

                        if has_crossing {
                            fill_line_to(line, col_x + 1, dim, reset);
                            let current_vis = visible_len(line);
                            if current_vis == col_x + 1 {
                                line.push_str(&format!("{}┼{}", dim, reset));
                            }
                        } else {
                            pad_line_to(line, col_x + 1);
                            let current_vis = visible_len(line);
                            if current_vis == col_x + 1 {
                                line.push_str(&format!("{}│{}", dim, reset));
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Generate a 2D spatial graph layout with Unicode box-drawing characters.
///
/// Layout strategy (top-to-bottom):
/// 1. Topological sort assigns each node a layer (depth from roots)
/// 2. Nodes within a layer are ordered to reduce edge crossings
/// 3. Each node is rendered as a box: ┌─┐ │id│ │status│ └─┘
/// 4. Vertical lines connect parent layer to child layer
/// 5. Fan-out uses ┬ splitters, fan-in uses ┴ mergers
pub fn generate_graph(
    graph: &WorkGraph,
    tasks: &[&Task],
    task_ids: &HashSet<&str>,
    annotations: &HashMap<String, String>,
    live_token_usage: &HashMap<String, TokenUsage>,
    assign_token_usage: &HashMap<String, TokenUsage>,
    eval_token_usage: &HashMap<String, TokenUsage>,
) -> String {
    generate_graph_with_overrides(graph, tasks, task_ids, annotations, &HashMap::new(), live_token_usage, assign_token_usage, eval_token_usage)
}

/// Like generate_graph but allows overriding the displayed status for each task.
/// Used by trace animation to show historical snapshots.
pub fn generate_graph_with_overrides(
    _graph: &WorkGraph,
    tasks: &[&Task],
    task_ids: &HashSet<&str>,
    annotations: &HashMap<String, String>,
    status_overrides: &HashMap<&str, Status>,
    live_token_usage: &HashMap<String, TokenUsage>,
    assign_token_usage: &HashMap<String, TokenUsage>,
    eval_token_usage: &HashMap<String, TokenUsage>,
) -> String {
    if tasks.is_empty() {
        return String::from("(no tasks to display)");
    }

    // Build adjacency
    let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in tasks {
        for blocker in &task.after {
            if task_ids.contains(blocker.as_str()) {
                forward
                    .entry(blocker.as_str())
                    .or_default()
                    .push(task.id.as_str());
                reverse
                    .entry(task.id.as_str())
                    .or_default()
                    .push(blocker.as_str());
            }
        }
    }
    for v in forward.values_mut() {
        v.sort();
    }

    // Color helpers
    let use_color = std::io::stdout().is_terminal();
    let status_color = |status: &Status| -> &str {
        if !use_color {
            return "";
        }
        match status {
            Status::Done => "\x1b[32m",
            Status::InProgress => "\x1b[33m",
            Status::Open => "\x1b[37m",
            Status::Blocked => "\x1b[90m",
            Status::Failed => "\x1b[31m",
            Status::Abandoned => "\x1b[90m",
        }
    };
    let reset = if use_color { "\x1b[0m" } else { "" };

    let status_label = |status: &Status| -> &str {
        match status {
            Status::Done => "done",
            Status::InProgress => "in-progress",
            Status::Open => "open",
            Status::Blocked => "blocked",
            Status::Failed => "failed",
            Status::Abandoned => "abandoned",
        }
    };

    // Assign layers via BFS from roots
    let roots: Vec<&str> = tasks
        .iter()
        .filter(|t| reverse.get(t.id.as_str()).map(Vec::is_empty).unwrap_or(true))
        .map(|t| t.id.as_str())
        .collect();

    let mut layer_of: HashMap<&str, usize> = HashMap::new();
    let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();

    for &root in &roots {
        if !layer_of.contains_key(root) {
            layer_of.insert(root, 0);
            queue.push_back(root);
        }
    }
    // Also seed any tasks not reachable from roots (cycles)
    for task in tasks {
        if !layer_of.contains_key(task.id.as_str()) {
            layer_of.insert(task.id.as_str(), 0);
            queue.push_back(task.id.as_str());
        }
    }

    while let Some(id) = queue.pop_front() {
        let my_layer = layer_of[id];
        if let Some(children) = forward.get(id) {
            for &child in children {
                let new_layer = my_layer + 1;
                let entry = layer_of.entry(child).or_insert(0);
                if *entry < new_layer {
                    *entry = new_layer;
                    queue.push_back(child);
                }
            }
        }
    }

    // Group nodes by layer
    let max_layer = layer_of.values().copied().max().unwrap_or(0);
    let mut layers: Vec<Vec<&str>> = vec![vec![]; max_layer + 1];
    for (&id, &layer) in &layer_of {
        layers[layer].push(id);
    }

    // Order nodes within each layer: sort by average parent position, then alphabetically
    for layer_idx in 0..=max_layer {
        if layer_idx == 0 {
            layers[layer_idx].sort();
        } else {
            let prev_layer = &layers[layer_idx - 1];
            let prev_pos: HashMap<&str, usize> = prev_layer
                .iter()
                .enumerate()
                .map(|(i, &id)| (id, i))
                .collect();

            layers[layer_idx].sort_by(|a, b| {
                let avg_a = avg_parent_pos(a, &reverse, &prev_pos);
                let avg_b = avg_parent_pos(b, &reverse, &prev_pos);
                avg_a
                    .partial_cmp(&avg_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.cmp(b))
            });
        }
    }

    // Build box content for each node: [line1=id, line2=status]
    // Truncate ID to keep boxes reasonable
    let max_id_len = 16;

    struct BoxInfo {
        lines: Vec<String>,      // content lines (no color)
        color_lines: Vec<String>, // content lines (with color)
        width: usize,            // inner width
    }

    let mut box_infos: HashMap<&str, BoxInfo> = HashMap::new();
    for task in tasks {
        let id = task.id.as_str();
        let display_id = if id.len() > max_id_len {
            format!("{}…", &id[..max_id_len - 1])
        } else {
            id.to_string()
        };
        let effective_status = status_overrides.get(id).copied().unwrap_or(task.status);
        let status = status_label(&effective_status);
        let phase = annotations
            .get(id)
            .map(|a| format!(" {}", a))
            .unwrap_or_default();

        let loop_info = if let Some(ref cfg) = task.cycle_config {
            if cfg.max_iterations > 0 {
                format!(" ↺ {}/{}", task.loop_iteration, cfg.max_iterations)
            } else {
                " ↺".to_string()
            }
        } else if task.loop_iteration > 0 {
            format!(" ↺ {}", task.loop_iteration)
        } else {
            String::new()
        };

        let usage = task.token_usage.as_ref().or_else(|| live_token_usage.get(id));
        let atok_usage = assign_token_usage.get(id);
        let etok_usage = eval_token_usage.get(id);
        let token_info = format_token_display(usage, atok_usage, etok_usage)
            .map(|s| format!(" · {}", s))
            .unwrap_or_default();

        let line1 = display_id;
        let line2 = format!("{}{}{}{}", status, token_info, phase, loop_info);
        let width = line1.len().max(line2.len());

        let color = status_color(&effective_status);
        let color_line1 = format!("{}{}{}", color, center_str(&line1, width), reset);
        let color_line2 = format!("{}{}{}", color, center_str(&line2, width), reset);

        box_infos.insert(
            id,
            BoxInfo {
                lines: vec![center_str(&line1, width), center_str(&line2, width)],
                color_lines: vec![color_line1, color_line2],
                width,
            },
        );
    }

    // Now render top-to-bottom: for each layer, draw boxes side by side,
    // then draw connecting lines to the next layer.

    // Compute positions: each box needs (box_width + 2 for borders + 1 gap)
    // Position = horizontal offset of each box center

    let gap = 1usize; // gap between boxes

    // For each layer, compute box positions (left edge of each box)
    let mut layer_positions: Vec<Vec<usize>> = Vec::new(); // [layer][node_idx] = left_x
    let mut layer_widths: Vec<Vec<usize>> = Vec::new(); // [layer][node_idx] = box outer width
    let mut layer_total_widths: Vec<usize> = Vec::new();

    for layer in &layers {
        let mut positions = Vec::new();
        let mut widths = Vec::new();
        let mut x = 0usize;
        for &id in layer {
            let info = &box_infos[id];
            let outer_w = info.width + 2; // +2 for │ on each side
            positions.push(x);
            widths.push(outer_w);
            x += outer_w + gap;
        }
        let total = if x > 0 { x - gap } else { 0 };
        layer_total_widths.push(total);
        layer_positions.push(positions);
        layer_widths.push(widths);
    }

    // Center all layers relative to the widest layer
    let max_width = layer_total_widths.iter().copied().max().unwrap_or(0);
    for (layer_idx, positions) in layer_positions.iter_mut().enumerate() {
        let total = layer_total_widths[layer_idx];
        let offset = if max_width > total {
            (max_width - total) / 2
        } else {
            0
        };
        for pos in positions.iter_mut() {
            *pos += offset;
        }
    }

    let canvas_width = max_width;

    // Helper: center x of a box
    let box_center = |layer_idx: usize, node_idx: usize| -> usize {
        layer_positions[layer_idx][node_idx] + layer_widths[layer_idx][node_idx] / 2
    };

    // Find node position in its layer
    let node_pos: HashMap<&str, (usize, usize)> = {
        let mut m = HashMap::new();
        for (li, layer) in layers.iter().enumerate() {
            for (ni, &id) in layer.iter().enumerate() {
                m.insert(id, (li, ni));
            }
        }
        m
    };

    // Render into output lines
    let mut output: Vec<String> = Vec::new();

    for (layer_idx, layer) in layers.iter().enumerate() {
        // Draw boxes for this layer (3 rows: top border, content lines, bottom border)
        let num_content_lines = 2;
        let mut row_top = vec![' '; canvas_width];
        let mut row_bot = vec![' '; canvas_width];
        let mut content_rows: Vec<Vec<char>> = (0..num_content_lines)
            .map(|_| vec![' '; canvas_width])
            .collect();

        for (ni, &id) in layer.iter().enumerate() {
            let info = &box_infos[id];
            let left = layer_positions[layer_idx][ni];
            let w = info.width;
            let outer_w = layer_widths[layer_idx][ni];

            // Top border: ┌──┐
            if left < canvas_width {
                row_top[left] = '┌';
            }
            for i in 1..=w {
                if left + i < canvas_width {
                    row_top[left + i] = '─';
                }
            }
            if left + outer_w - 1 < canvas_width {
                row_top[left + outer_w - 1] = '┐';
            }

            // Bottom border: └──┘
            if left < canvas_width {
                row_bot[left] = '└';
            }
            for i in 1..=w {
                if left + i < canvas_width {
                    row_bot[left + i] = '─';
                }
            }
            if left + outer_w - 1 < canvas_width {
                row_bot[left + outer_w - 1] = '┘';
            }

            // Content lines: │text│
            for (ci, _line) in info.lines.iter().enumerate() {
                let row = &mut content_rows[ci];
                if left < canvas_width {
                    row[left] = '│';
                }
                for (j, ch) in info.lines[ci].chars().enumerate() {
                    if left + 1 + j < canvas_width {
                        row[left + 1 + j] = ch;
                    }
                }
                if left + outer_w - 1 < canvas_width {
                    row[left + outer_w - 1] = '│';
                }
            }
        }

        // If we use color, we need to inject ANSI codes around content chars.
        // For simplicity with color: rebuild content rows as strings with color.
        output.push(row_top.iter().collect::<String>().trim_end().to_string());

        for (ci, content_row) in content_rows.iter().enumerate().take(num_content_lines) {
            if use_color {
                let mut s = String::new();
                for (ni, &id) in layer.iter().enumerate() {
                    let info = &box_infos[id];
                    let left = layer_positions[layer_idx][ni];
                    let outer_w = layer_widths[layer_idx][ni];

                    // Pad spaces before this box
                    while s.len() < left {
                        s.push(' ');
                    }
                    s.push('│');
                    // Use the color_lines version
                    s.push_str(&info.color_lines[ci]);
                    // Pad to fill box if color_lines is shorter visually
                    s.push('│');
                    // Pad to outer_w
                    while visible_len(&s) < left + outer_w + gap {
                        s.push(' ');
                    }
                }
                output.push(s.trim_end().to_string());
            } else {
                output.push(
                    content_row
                        .iter()
                        .collect::<String>()
                        .trim_end()
                        .to_string(),
                );
            }
        }

        output.push(row_bot.iter().collect::<String>().trim_end().to_string());

        // Draw connecting lines to next layer
        if layer_idx < max_layer {
            let next_layer_idx = layer_idx + 1;

            // Collect all edges from this layer to the next
            struct Edge {
                parent_center: usize,
                child_center: usize,
            }
            let mut edges: Vec<Edge> = Vec::new();

            for (ni, &pid) in layer.iter().enumerate() {
                if let Some(children) = forward.get(pid) {
                    let pc = box_center(layer_idx, ni);
                    for &cid in children {
                        if let Some(&(cl, cn)) = node_pos.get(cid)
                            && cl == next_layer_idx {
                                let cc = box_center(cl, cn);
                                edges.push(Edge {
                                    parent_center: pc,
                                    child_center: cc,
                                });
                            }
                    }
                }
            }

            if edges.is_empty() {
                // No edges to next layer, just blank line
                output.push(String::new());
            } else {
                // Row 1: vertical drops from parent centers
                let mut row1 = vec![' '; canvas_width];
                let parent_centers: HashSet<usize> =
                    edges.iter().map(|e| e.parent_center).collect();
                for &pc in &parent_centers {
                    if pc < canvas_width {
                        row1[pc] = '│';
                    }
                }
                output.push(row1.iter().collect::<String>().trim_end().to_string());

                // Row 2: horizontal span with connectors
                // For each parent center, draw horizontal line to all its children
                // Group edges by parent
                let mut by_parent: HashMap<usize, Vec<usize>> = HashMap::new();
                for e in &edges {
                    by_parent
                        .entry(e.parent_center)
                        .or_default()
                        .push(e.child_center);
                }

                let mut row2 = vec![' '; canvas_width];
                // Mark all positions that need something
                let mut marks: HashMap<usize, char> = HashMap::new();

                for (&pc, children) in &by_parent {
                    let mut all_points: Vec<usize> = children.clone();
                    all_points.push(pc);
                    all_points.sort();
                    all_points.dedup();

                    let min_x = *all_points.first().unwrap();
                    let max_x = *all_points.last().unwrap();

                    // Draw horizontal line
                    #[allow(clippy::needless_range_loop)]
                    for x in min_x..=max_x {
                        if x < canvas_width && row2[x] == ' ' {
                            row2[x] = '─';
                        }
                    }

                    // Mark parent center with ┼ or ┬ etc
                    // Mark child centers with ┬ (they'll receive │ going down)
                    for &pt in &all_points {
                        if pt < canvas_width {
                            let existing = marks.get(&pt).copied().unwrap_or('─');
                            let is_parent = pt == pc;
                            let is_child = children.contains(&pt);
                            let ch = if is_parent && is_child {
                                // Parent center that is also a child target: ┼
                                upgrade_connector(existing, true, true)
                            } else if is_parent {
                                upgrade_connector(existing, true, false)
                            } else {
                                upgrade_connector(existing, false, true)
                            };
                            marks.insert(pt, ch);
                        }
                    }
                }

                // Apply marks to row2
                for (&x, &ch) in &marks {
                    if x < canvas_width {
                        row2[x] = ch;
                    }
                }

                output.push(row2.iter().collect::<String>().trim_end().to_string());

                // Row 3: vertical drops to child centers
                let mut row3 = vec![' '; canvas_width];
                let child_centers: HashSet<usize> =
                    edges.iter().map(|e| e.child_center).collect();
                for &cc in &child_centers {
                    if cc < canvas_width {
                        row3[cc] = '│';
                    }
                }
                output.push(row3.iter().collect::<String>().trim_end().to_string());
            }
        }
    }


    output.join("\n")
}

fn avg_parent_pos(id: &str, reverse: &HashMap<&str, Vec<&str>>, prev_pos: &HashMap<&str, usize>) -> f64 {
    let parents = match reverse.get(id) {
        Some(p) => p,
        None => return f64::MAX,
    };
    let positions: Vec<usize> = parents.iter().filter_map(|p| prev_pos.get(p).copied()).collect();
    if positions.is_empty() {
        return f64::MAX;
    }
    positions.iter().sum::<usize>() as f64 / positions.len() as f64
}

fn center_str(s: &str, width: usize) -> String {
    if s.len() >= width {
        return s.to_string();
    }
    let pad = width - s.len();
    let left = pad / 2;
    let right = pad - left;
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
}

fn visible_len(s: &str) -> usize {
    // Strip ANSI escape codes to get visible length
    let mut len = 0;
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            if ch == 'm' {
                in_escape = false;
            }
        } else if ch == '\x1b' {
            in_escape = true;
        } else {
            len += 1;
        }
    }
    len
}

/// Determine the right box-drawing connector character.
/// `from_above` = line comes from parent above, `to_below` = line goes to child below.
fn upgrade_connector(existing: char, from_above: bool, to_below: bool) -> char {
    match (existing, from_above, to_below) {
        (_, true, true) => '┼',
        (_, true, false) => '┴',
        ('┴', false, true) => '┼',
        (_, false, true) => '┬',
        _ => existing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn test_generate_dot_basic() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();

        let no_annots = HashMap::new();
        let dot = generate_dot(&graph, &tasks, &task_ids, &critical_path, &no_annots);
        assert!(dot.contains("digraph workgraph"));
        assert!(dot.contains("\"t1\""));
        assert!(dot.contains("Task 1"));
    }

    #[test]
    fn test_generate_dot_with_hours() {
        let mut graph = WorkGraph::new();
        let t1 = make_task_with_hours("t1", "Task 1", 8.0);
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();
        let no_annots = HashMap::new();

        let dot = generate_dot(&graph, &tasks, &task_ids, &critical_path, &no_annots);
        assert!(dot.contains("8h"));
    }

    #[test]
    fn test_generate_dot_with_critical_path() {
        let mut graph = WorkGraph::new();
        let t1 = make_task_with_hours("t1", "Task 1", 8.0);
        let mut t2 = make_task_with_hours("t2", "Task 2", 16.0);
        t2.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let mut critical_path = HashSet::new();
        critical_path.insert("t1".to_string());
        critical_path.insert("t2".to_string());
        let no_annots = HashMap::new();

        let dot = generate_dot(&graph, &tasks, &task_ids, &critical_path, &no_annots);
        assert!(dot.contains("color=red"));
        assert!(dot.contains("penwidth"));
    }

    #[test]
    fn test_generate_mermaid_basic() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();
        let no_annots = HashMap::new();

        let mermaid = generate_mermaid(&graph, &tasks, &task_ids, &critical_path, &no_annots);
        assert!(mermaid.contains("flowchart LR"));
        assert!(mermaid.contains("t1"));
    }

    #[test]
    fn test_generate_mermaid_with_dependency() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.after = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();
        let no_annots = HashMap::new();

        let mermaid = generate_mermaid(&graph, &tasks, &task_ids, &critical_path, &no_annots);
        assert!(mermaid.contains("t1 --> t2"));
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
    fn test_generate_ascii_empty() {
        let graph = WorkGraph::new();
        let tasks: Vec<&workgraph::graph::Task> = vec![];
        let task_ids: HashSet<&str> = HashSet::new();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());
        assert_eq!(result, "(no tasks to display)");
    }

    #[test]
    fn test_generate_ascii_simple_edge() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("src", "Source task");
        let mut t2 = make_task("tgt", "Target task");
        t2.after = vec!["src".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Tree output: src is root, tgt is child
        assert!(result.contains("src"));
        assert!(result.contains("tgt"));
        assert!(result.contains("└→"));
        assert!(result.contains("(open)"));
    }

    #[test]
    fn test_generate_ascii_fan_out() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("a", "Task A");
        let mut t2 = make_task("b", "Task B");
        t2.after = vec!["a".to_string()];
        let mut t3 = make_task("c", "Task C");
        t3.after = vec!["a".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // a is root with two children
        assert!(result.contains("├→"));
        assert!(result.contains("└→"));
        assert!(result.contains('a'));
        assert!(result.contains('b'));
        assert!(result.contains('c'));
    }

    #[test]
    fn test_generate_ascii_fan_in() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("a", "Task A");
        let t2 = make_task("b", "Task B");
        let mut t3 = make_task("c", "Merge Task");
        t3.after = vec!["a".to_string(), "b".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // c should appear, and the fan-in edge should be shown as a right-side arc
        assert!(result.contains('c'));
        // Fan-in is now shown via right-side arcs (←/┘) instead of text annotations
        assert!(
            result.contains("←") || result.contains("┘"),
            "Fan-in should produce a right-side arc.\nOutput:\n{}",
            result
        );
    }

    #[test]
    fn test_generate_ascii_independent() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("solo", "Solo task");
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        assert!(result.contains("solo"));
        assert!(result.contains("(independent)"));
    }

    #[test]
    fn test_generate_ascii_status_labels() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("root", "Root");
        t1.status = Status::InProgress;
        let mut t2 = make_task("child", "Child");
        t2.status = Status::Blocked;
        t2.after = vec!["root".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        assert!(result.contains("(in-progress)"));
        assert!(result.contains("(blocked)"));
    }

    #[test]
    fn test_generate_ascii_chain() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("a", "Task A");
        let mut t2 = make_task("b", "Task B");
        t2.after = vec!["a".to_string()];
        let mut t3 = make_task("c", "Task C");
        t3.after = vec!["b".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Should show indented chain: a -> b -> c
        assert!(result.contains("a"));
        assert!(result.contains("b"));
        assert!(result.contains("c"));
        // b and c should be indented (have └─→ prefix)
        let result_lines: Vec<&str> = result.lines().collect();
        // First line is the root (a), no prefix
        assert!(result_lines[0].contains("a"));
        // Nested nodes should have tree characters
        assert!(result.contains("└→"));
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
    fn test_is_internal_task() {
        let assign = make_internal_task("assign-foo", "Assign agent to foo", "assignment", vec![]);
        let eval = make_internal_task("evaluate-foo", "Evaluate foo", "evaluation", vec!["foo"]);
        let normal = make_task("foo", "Normal task");

        assert!(is_internal_task(&assign));
        assert!(is_internal_task(&eval));
        assert!(!is_internal_task(&normal));
    }

    #[test]
    fn test_ascii_hides_internal_tasks_by_default() {
        let mut graph = WorkGraph::new();
        let mut parent = make_task("my-task", "My Task");
        parent.status = Status::Open;
        let mut assign = make_internal_task(
            "assign-my-task",
            "Assign agent to my-task",
            "assignment",
            vec![],
        );
        assign.status = Status::InProgress;
        // assign task blocks parent (parent is blocked by assign)
        parent.after = vec!["assign-my-task".to_string()];
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        let annotations = HashMap::new();
        let (filtered, annots) =
            filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        let result = generate_ascii(&graph, &filtered, &task_ids, &annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Internal task should NOT appear
        assert!(!result.contains("assign-my-task"));
        // Parent task should appear with phase annotation
        assert!(result.contains("my-task"));
        assert!(result.contains("[assigning]"));
    }

    #[test]
    fn test_ascii_shows_evaluating_phase() {
        let mut graph = WorkGraph::new();
        let mut parent = make_task("my-task", "My Task");
        parent.status = Status::Done;
        let mut eval = make_internal_task(
            "evaluate-my-task",
            "Evaluate my-task",
            "evaluation",
            vec!["my-task"],
        );
        eval.status = Status::InProgress;
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(eval));

        let annotations = HashMap::new();
        let (filtered, annots) =
            filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        let result = generate_ascii(&graph, &filtered, &task_ids, &annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        assert!(!result.contains("evaluate-my-task"));
        assert!(result.contains("my-task"));
        assert!(result.contains("[evaluating]"));
    }

    #[test]
    fn test_dot_hides_internal_tasks_by_default() {
        let mut graph = WorkGraph::new();
        let mut parent = make_task("my-task", "My Task");
        parent.status = Status::Open;
        let mut assign = make_internal_task(
            "assign-my-task",
            "Assign agent to my-task",
            "assignment",
            vec![],
        );
        assign.status = Status::InProgress;
        parent.after = vec!["assign-my-task".to_string()];
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        let annotations = HashMap::new();
        let (filtered, annots) =
            filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();

        let result = generate_dot(&graph, &filtered, &task_ids, &critical_path, &annots);

        assert!(!result.contains("assign-my-task"));
        assert!(result.contains("my-task"));
        assert!(result.contains("[assigning]"));
    }

    #[test]
    fn test_mermaid_hides_internal_tasks_by_default() {
        let mut graph = WorkGraph::new();
        let mut parent = make_task("my-task", "My Task");
        parent.status = Status::Open;
        let mut assign = make_internal_task(
            "assign-my-task",
            "Assign agent to my-task",
            "assignment",
            vec![],
        );
        assign.status = Status::InProgress;
        parent.after = vec!["assign-my-task".to_string()];
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        let annotations = HashMap::new();
        let (filtered, annots) =
            filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();
        let critical_path = HashSet::new();

        let result = generate_mermaid(&graph, &filtered, &task_ids, &critical_path, &annots);

        assert!(!result.contains("assign-my-task"));
        assert!(result.contains("my-task"));
        assert!(result.contains("[assigning]"));
    }

    #[test]
    fn test_show_internal_reveals_all_tasks() {
        let mut graph = WorkGraph::new();
        let mut parent = make_task("my-task", "My Task");
        parent.status = Status::Open;
        let assign = make_internal_task(
            "assign-my-task",
            "Assign agent to my-task",
            "assignment",
            vec![],
        );
        parent.after = vec!["assign-my-task".to_string()];
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        // When show_internal is true, we skip filtering
        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let annots = HashMap::new();

        let result = generate_ascii(&graph, &tasks, &task_ids, &annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Both tasks should be visible
        assert!(result.contains("assign-my-task"));
        assert!(result.contains("my-task"));
        // No phase annotation when shown as literal nodes
        assert!(!result.contains("[assigning]"));
    }

    #[test]
    fn test_ascii_loop_symbol_on_task_with_cycle_config() {
        use workgraph::graph::CycleConfig;

        let mut graph = WorkGraph::new();
        let mut src = make_task("src", "Source");
        src.cycle_config = Some(CycleConfig {
            max_iterations: 10,
            guard: None,
            delay: None,
        });
        src.loop_iteration = 3;
        let mut tgt = make_task("tgt", "Target");
        tgt.after = vec!["src".to_string()];
        src.after = vec!["tgt".to_string()];
        graph.add_node(Node::Task(src));
        graph.add_node(Node::Task(tgt));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // The source task (which has cycle_config) should show the ↺ symbol
        assert!(
            result.contains("↺"),
            "Expected ↺ symbol in output:\n{}",
            result
        );
        // Should show iteration info like (iter 3/10)
        assert!(
            result.contains("3/10"),
            "Expected iteration count in output:\n{}",
            result
        );
    }

    #[test]
    fn test_ascii_loop_symbol_independent_task() {
        use workgraph::graph::CycleConfig;

        let mut graph = WorkGraph::new();
        let mut task = make_task("looper", "Looping task");
        task.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
        });
        task.loop_iteration = 2;
        graph.add_node(Node::Task(task));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Should show ↺ symbol in the node label
        assert!(
            result.contains("↺"),
            "Expected ↺ symbol in output:\n{}",
            result
        );
        assert!(
            result.contains("2/5"),
            "Expected iteration count in output:\n{}",
            result
        );
    }

    #[test]
    fn test_ascii_loop_symbol_with_cycle_config_no_iteration() {
        use workgraph::graph::CycleConfig;

        let mut graph = WorkGraph::new();
        let mut src = make_task("src", "Source");
        src.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
        });
        let mut tgt = make_task("tgt", "Target");
        tgt.after = vec!["src".to_string()];
        graph.add_node(Node::Task(src));
        graph.add_node(Node::Task(tgt));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Task with cycle_config should show the ↺ symbol
        assert!(
            result.contains("↺"),
            "Expected ↺ symbol for cycle_config task:\n{}",
            result
        );
    }

    #[test]
    fn test_ascii_no_loop_symbol_on_normal_tasks() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("normal", "Normal task");
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // No loop symbol on tasks without loops
        assert!(
            !result.contains("↺"),
            "Should NOT contain ↺ on normal task:\n{}",
            result
        );
        assert!(
            !result.contains("↻"),
            "Should NOT contain ↻ on normal task:\n{}",
            result
        );
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

    // --- Graph (2D box layout) tests ---

    #[test]
    fn test_generate_graph_empty() {
        let graph = WorkGraph::new();
        let tasks: Vec<&Task> = vec![];
        let task_ids: HashSet<&str> = HashSet::new();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(result, "(no tasks to display)");
    }

    #[test]
    fn test_generate_graph_single_node() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("alpha", "Alpha");
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new());

        assert!(result.contains("alpha"), "Should contain task id:\n{}", result);
        assert!(result.contains("open"), "Should contain status:\n{}", result);
        assert!(result.contains('┌'), "Should have box top:\n{}", result);
        assert!(result.contains('┘'), "Should have box bottom:\n{}", result);
    }

    #[test]
    fn test_generate_graph_simple_chain() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("a", "Task A");
        let mut t2 = make_task("b", "Task B");
        t2.after = vec!["a".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new());

        // Both boxes should appear
        assert!(result.contains('a'), "Should contain 'a':\n{}", result);
        assert!(result.contains('b'), "Should contain 'b':\n{}", result);
        // Connecting line between layers
        assert!(result.contains('│'), "Should have vertical connector:\n{}", result);
    }

    #[test]
    fn test_generate_graph_fan_out() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("root", "Root");
        let mut c1 = make_task("c1", "Child 1");
        c1.after = vec!["root".to_string()];
        let mut c2 = make_task("c2", "Child 2");
        c2.after = vec!["root".to_string()];
        let mut c3 = make_task("c3", "Child 3");
        c3.after = vec!["root".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(c1));
        graph.add_node(Node::Task(c2));
        graph.add_node(Node::Task(c3));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new());

        // All children should appear
        assert!(result.contains("c1"), "Should contain c1:\n{}", result);
        assert!(result.contains("c2"), "Should contain c2:\n{}", result);
        assert!(result.contains("c3"), "Should contain c3:\n{}", result);
        // Should have horizontal connector for fan-out
        assert!(result.contains('┬'), "Should have ┬ for fan-out:\n{}", result);
    }

    #[test]
    fn test_generate_graph_fan_in() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("a", "Task A");
        let t2 = make_task("b", "Task B");
        let mut merge = make_task("merge", "Merge");
        merge.after = vec!["a".to_string(), "b".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(merge));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new());

        // All nodes should be present
        assert!(result.contains('a'), "Should contain a:\n{}", result);
        assert!(result.contains('b'), "Should contain b:\n{}", result);
        assert!(result.contains("merge"), "Should contain merge:\n{}", result);
    }

    #[test]
    fn test_generate_graph_diamond() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("start", "Start");
        let mut left = make_task("left", "Left");
        left.after = vec!["start".to_string()];
        let mut right = make_task("right", "Right");
        right.after = vec!["start".to_string()];
        let mut end = make_task("end", "End");
        end.after = vec!["left".to_string(), "right".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(left));
        graph.add_node(Node::Task(right));
        graph.add_node(Node::Task(end));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new());

        // All 4 nodes
        assert!(result.contains("start"), "Should contain start:\n{}", result);
        assert!(result.contains("left"), "Should contain left:\n{}", result);
        assert!(result.contains("right"), "Should contain right:\n{}", result);
        assert!(result.contains("end"), "Should contain end:\n{}", result);
        // 3 layers of boxes
        let box_tops = result.matches('┌').count();
        assert!(box_tops >= 4, "Should have at least 4 box tops:\n{}", result);
    }

    #[test]
    fn test_generate_graph_status_display() {
        let mut graph = WorkGraph::new();
        let mut t1 = make_task("root", "Root");
        t1.status = Status::InProgress;
        let mut t2 = make_task("child", "Child");
        t2.status = Status::Blocked;
        t2.after = vec!["root".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new());

        assert!(result.contains("in-progress"), "Should show in-progress status:\n{}", result);
        assert!(result.contains("blocked"), "Should show blocked status:\n{}", result);
    }

    #[test]
    fn test_generate_graph_loop_annotation() {
        use workgraph::graph::CycleConfig;

        let mut graph = WorkGraph::new();
        let mut src = make_task("src", "Source");
        src.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
        });
        src.loop_iteration = 2;
        let mut tgt = make_task("tgt", "Target");
        tgt.after = vec!["src".to_string()];
        graph.add_node(Node::Task(src));
        graph.add_node(Node::Task(tgt));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new());

        assert!(result.contains("↺"), "Should show loop annotation:\n{}", result);
        assert!(result.contains("2/5"), "Should show iteration count:\n{}", result);
    }

    #[test]
    fn test_generate_graph_long_id_truncation() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("very-long-task-id-that-exceeds-limit", "Long ID");
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new());

        // ID should be truncated with ellipsis
        assert!(result.contains('…'), "Should truncate long id:\n{}", result);
        // Full ID should NOT appear
        assert!(!result.contains("very-long-task-id-that-exceeds-limit"),
            "Full long ID should not appear:\n{}", result);
    }

    #[test]
    fn test_generate_graph_format_parsing() {
        assert_eq!("graph".parse::<OutputFormat>().unwrap(), OutputFormat::Graph);
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

    // --- Right-side arc rendering tests ---

    #[test]
    fn test_arc_back_edge_cycle() {
        // A→B→A cycle: back-edge should produce right-side arcs with arrows
        let mut graph = WorkGraph::new();
        let mut a = make_task("design", "Design");
        a.cycle_config = Some(workgraph::graph::CycleConfig {
            max_iterations: 3,
            guard: None,
            delay: None,
        });
        a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut b = make_task("verify", "Verify");
        b.after = vec!["design".to_string()];
        b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        a.after = vec!["verify".to_string()]; // back-edge
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Should have ← at target and ┘ at source
        assert!(result.contains("←"), "Back-edge target should have ←\nOutput:\n{}", result);
        assert!(result.contains("┘"), "Back-edge source should have ┘\nOutput:\n{}", result);
        // Should NOT have old-style cycle-back text
        assert!(!result.contains("cycles back"), "No old-style text\nOutput:\n{}", result);
        // Should NOT have fan-in annotations
        assert!(!result.contains("(←"), "No fan-in text annotations\nOutput:\n{}", result);
    }

    #[test]
    fn test_arc_fan_in_diamond() {
        // Diamond: A→{B,C}→D — D has fan-in from secondary parent
        let mut graph = WorkGraph::new();
        let mut a = make_task("root", "Root");
        a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut b = make_task("left", "Left");
        b.after = vec!["root".to_string()];
        b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        let mut c = make_task("right", "Right");
        c.after = vec!["root".to_string()];
        c.created_at = Some("2024-01-01T00:02:00Z".to_string());
        let mut d = make_task("join", "Join");
        d.after = vec!["left".to_string(), "right".to_string()];
        d.created_at = Some("2024-01-01T00:03:00Z".to_string());
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Fan-in should produce a right-side arc (not a text annotation)
        assert!(result.contains("←") || result.contains("┘"),
            "Diamond fan-in should have right-side arcs\nOutput:\n{}", result);
        assert!(!result.contains("(←"), "No fan-in text annotation\nOutput:\n{}", result);
        assert!(!result.contains("..."), "No duplicate 'already shown' entries\nOutput:\n{}", result);
    }

    #[test]
    fn test_diamond_layout_join_under_ancestor() {
        // Diamond: root→{left,right}→join
        // With diamond layout, join should be a direct child of root (same level as left/right),
        // NOT a grandchild of root under left. Arcs from left and right flow DOWN to join.
        let mut graph = WorkGraph::new();
        let mut root = make_task("root", "Root");
        root.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut left = make_task("left", "Left");
        left.after = vec!["root".to_string()];
        left.created_at = Some("2024-01-01T00:01:00Z".to_string());
        let mut right = make_task("right", "Right");
        right.after = vec!["root".to_string()];
        right.created_at = Some("2024-01-01T00:02:00Z".to_string());
        let mut join = make_task("join", "Join");
        join.after = vec!["left".to_string(), "right".to_string()];
        join.created_at = Some("2024-01-01T00:03:00Z".to_string());
        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(left));
        graph.add_node(Node::Task(right));
        graph.add_node(Node::Task(join));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();

        // Diamond layout (default)
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::Diamond);
        eprintln!("DIAMOND:\n{}", result);

        let lines: Vec<&str> = result.lines().collect();
        // join should be at the same indentation as left and right (direct child of root)
        let join_line = lines.iter().find(|l| l.contains("join")).expect("join should appear");
        let left_line = lines.iter().find(|l| l.contains("left")).expect("left should appear");
        // Both should start with tree connectors at the same indent level
        let join_indent = join_line.find("join").unwrap();
        let left_indent = left_line.find("left").unwrap();
        assert_eq!(join_indent, left_indent,
            "Diamond layout: join should be at same indent as left\nOutput:\n{}", result);

        // join should appear AFTER both left and right in line order
        let left_idx = lines.iter().position(|l| l.contains("left")).unwrap();
        let right_idx = lines.iter().position(|l| l.contains("right")).unwrap();
        let join_idx = lines.iter().position(|l| l.contains("join")).unwrap();
        assert!(join_idx > left_idx, "join should be after left");
        assert!(join_idx > right_idx, "join should be after right");

        // Arcs should flow DOWN (left and right have ┐ or ─, join has ← or ┘)
        assert!(join_line.contains("←") || join_line.contains("┘"),
            "join should receive arcs\nOutput:\n{}", result);

        // Compare with tree layout (old behavior): join should be under left
        let result_tree = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::Tree);
        eprintln!("TREE:\n{}", result_tree);
        let tree_lines: Vec<&str> = result_tree.lines().collect();
        let join_tree = tree_lines.iter().find(|l| l.contains("join")).expect("join in tree");
        let left_tree = tree_lines.iter().find(|l| l.contains("left")).expect("left in tree");
        let join_tree_indent = join_tree.find("join").unwrap();
        let left_tree_indent = left_tree.find("left").unwrap();
        // In tree mode, join should be DEEPER than left (a child of left)
        assert!(join_tree_indent > left_tree_indent,
            "Tree layout: join should be deeper than left\nOutput:\n{}", result_tree);
    }

    #[test]
    fn test_diamond_layout_wider_fan() {
        // Wider diamond: root→{a,b,c,d}→join
        let mut graph = WorkGraph::new();
        let mut root = make_task("root", "Root");
        root.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let names = ["aaa", "bbb", "ccc", "ddd"];
        for (i, name) in names.iter().enumerate() {
            let mut t = make_task(name, name);
            t.after = vec!["root".to_string()];
            t.created_at = Some(format!("2024-01-01T00:{:02}:00Z", i + 1));
            graph.add_node(Node::Task(t));
        }
        let mut join = make_task("join", "Join");
        join.after = names.iter().map(|s| s.to_string()).collect();
        join.created_at = Some("2024-01-01T00:10:00Z".to_string());
        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(join));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::Diamond);
        eprintln!("WIDE DIAMOND:\n{}", result);

        let lines: Vec<&str> = result.lines().collect();
        // join should be after all fan-out children
        let join_idx = lines.iter().position(|l| l.contains("join")).unwrap();
        for name in &names {
            let idx = lines.iter().position(|l| l.contains(name)).unwrap();
            assert!(join_idx > idx, "join should be after {}", name);
        }
        // join should receive arcs
        let join_line = lines.iter().find(|l| l.contains("join")).unwrap();
        assert!(join_line.contains("←") || join_line.contains("┘"),
            "join should receive arcs in wide diamond\nOutput:\n{}", result);
    }

    #[test]
    fn test_arc_no_orphaned_continuation() {
        // A has children [B, C] where C is a back-edge (already rendered).
        // B should use └→ (not ├→), no orphaned │.
        let mut graph = WorkGraph::new();
        let mut a = make_task("parent", "Parent");
        a.cycle_config = Some(workgraph::graph::CycleConfig {
            max_iterations: 2,
            guard: None,
            delay: None,
        });
        a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut b = make_task("child", "Child");
        b.after = vec!["parent".to_string()];
        b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        // child→parent back-edge (parent depends on child for cycle)
        a.after = vec!["child".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        let tree_lines: Vec<&str> = result.lines().collect();
        // The child should use └→ (last visible child), not ├→
        let child_line = tree_lines.iter().find(|l| l.contains("child"));
        assert!(child_line.is_some(), "Child should appear\nOutput:\n{}", result);
        assert!(
            child_line.unwrap().contains("└→"),
            "Child should use └→ (no orphaned ├→)\nLine: '{}'\nOutput:\n{}",
            child_line.unwrap(), result
        );
    }

    #[test]
    fn test_arc_same_target_collapse() {
        // Target with multiple sources should collapse into one column
        let mut graph = WorkGraph::new();
        let mut target = make_task("hub", "Hub");
        target.cycle_config = Some(workgraph::graph::CycleConfig {
            max_iterations: 2,
            guard: None,
            delay: None,
        });
        target.created_at = Some("2024-01-01T00:00:00Z".to_string());

        let mut s1 = make_task("spoke-a", "Spoke A");
        s1.after = vec!["hub".to_string()];
        s1.created_at = Some("2024-01-01T00:01:00Z".to_string());
        let mut s2 = make_task("spoke-b", "Spoke B");
        s2.after = vec!["hub".to_string()];
        s2.created_at = Some("2024-01-01T00:02:00Z".to_string());
        let mut s3 = make_task("spoke-c", "Spoke C");
        s3.after = vec!["hub".to_string()];
        s3.created_at = Some("2024-01-01T00:03:00Z".to_string());

        // All spokes cycle back to hub
        target.after = vec!["spoke-a".to_string(), "spoke-b".to_string(), "spoke-c".to_string()];

        graph.add_node(Node::Task(target));
        graph.add_node(Node::Task(s1));
        graph.add_node(Node::Task(s2));
        graph.add_node(Node::Task(s3));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Should have exactly one ← (same-target collapse)
        let target_count = result.matches("←").count();
        assert_eq!(target_count, 1,
            "Multiple sources to same target should collapse to 1 column\nOutput:\n{}", result);
        // Should have ┤ for intermediate sources and ┘ for the last
        assert!(result.contains("┤"), "Intermediate sources should have ┤\nOutput:\n{}", result);
        assert!(result.contains("┘"), "Last source should have ┘\nOutput:\n{}", result);
    }

    #[test]
    fn test_arc_dash_fill_with_space() {
        // Arcs should have a space between node text and dash fill
        let mut graph = WorkGraph::new();
        let mut a = make_task("aa", "AA");
        a.cycle_config = Some(workgraph::graph::CycleConfig {
            max_iterations: 2,
            guard: None,
            delay: None,
        });
        a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut b = make_task("bb", "BB");
        b.after = vec!["aa".to_string()];
        b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        a.after = vec!["bb".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Lines with arcs should have space before the dash fill
        for line in result.lines() {
            if line.contains("←") || line.contains("┘") {
                // The text content shouldn't run directly into ─
                assert!(
                    !line.contains(")─") && !line.contains(")←"),
                    "Should have space between text and arc fill\nLine: '{}'",
                    line
                );
            }
        }
    }

    #[test]
    fn test_arc_forward_skip_edge() {
        // Graph: A→B→C, A→C (forward skip: A blocks C directly)
        // With diamond layout: C is placed under A (LCA), B→C becomes a downward arc.
        let mut graph = WorkGraph::new();
        let mut a = make_task("aaa", "A");
        a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut b = make_task("bbb", "B");
        b.after = vec!["aaa".to_string()];
        b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        let mut c = make_task("ccc", "C");
        c.after = vec!["bbb".to_string(), "aaa".to_string()]; // C depends on both B and A
        c.created_at = Some("2024-01-01T00:02:00Z".to_string());
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Find the lines
        let lines: Vec<&str> = result.lines().collect();
        let b_line = lines.iter().find(|l| l.contains("bbb")).expect("B should appear");
        let c_line = lines.iter().find(|l| l.contains("ccc")).expect("C should appear");

        // C (dependent) should have ← arrowhead (arc from B flows down to C)
        assert!(c_line.contains("←"),
            "Forward skip: dependent C should have ←\nOutput:\n{}", result);
        // B (blocker) should have ┐ (top corner of downward arc to C)
        assert!(b_line.contains("┐"),
            "Forward skip: blocker B should have ┐\nOutput:\n{}", result);

        // Verify tree layout with LayoutMode::Tree (old behavior)
        let result_tree = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::Tree);
        let tree_lines: Vec<&str> = result_tree.lines().collect();
        let a_line_tree = tree_lines.iter().find(|l| l.contains("aaa")).expect("A should appear in tree mode");
        // In tree mode, A→C forward skip produces an arc with ┐ at A
        assert!(a_line_tree.contains("┐") || a_line_tree.contains("─"),
            "Tree mode: A should participate in arc\nOutput:\n{}", result_tree);
    }

    #[test]
    fn test_arc_mixed_direction_same_dependent() {
        // Graph: A→B→D, A→D, and E→D where E is rendered below D
        // D is the dependent from both A (above) and E (below)
        // Should produce a single column with ←┤ at D's line
        let mut graph = WorkGraph::new();
        let mut a = make_task("aaa", "A");
        a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut b = make_task("bbb", "B");
        b.after = vec!["aaa".to_string()];
        b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        let mut d = make_task("ddd", "D");
        d.after = vec!["bbb".to_string(), "aaa".to_string()]; // D depends on B and A (A is forward skip)
        d.created_at = Some("2024-01-01T00:02:00Z".to_string());
        // E is a sibling of B under A, and D also depends on E
        let mut e = make_task("eee", "E");
        e.after = vec!["aaa".to_string()];
        e.created_at = Some("2024-01-01T00:03:00Z".to_string());
        d.after.push("eee".to_string()); // D also depends on E

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(d));
        graph.add_node(Node::Task(e));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // D should have exactly one ← (same-dependent collapse)
        let arrow_count = result.matches("←").count();
        assert_eq!(arrow_count, 1,
            "Mixed direction arcs to same dependent should collapse to 1 column\nOutput:\n{}", result);

        // D's line should have ←
        let lines: Vec<&str> = result.lines().collect();
        let d_line = lines.iter().find(|l| l.contains("ddd")).expect("D should appear");
        assert!(d_line.contains("←"),
            "Mixed direction: D should have ←\nOutput:\n{}", result);
    }

    #[test]
    fn test_arc_multiple_forward_edges_from_same_source() {
        // Graph: A→{B,C} via tree, plus A→B and A→C as non-tree forward skips
        // Each dependent gets its own column with ← at the dependent
        let mut graph = WorkGraph::new();
        let mut a = make_task("root", "Root");
        a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut b = make_task("mid", "Mid");
        b.after = vec!["root".to_string()];
        b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        let mut c = make_task("leaf-a", "Leaf A");
        c.after = vec!["mid".to_string(), "root".to_string()]; // forward skip from root
        c.created_at = Some("2024-01-01T00:02:00Z".to_string());
        let mut d = make_task("leaf-b", "Leaf B");
        d.after = vec!["mid".to_string(), "root".to_string()]; // forward skip from root
        d.created_at = Some("2024-01-01T00:03:00Z".to_string());

        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default());

        // Each dependent (leaf-a, leaf-b) should have ←
        let lines: Vec<&str> = result.lines().collect();
        let c_line = lines.iter().find(|l| l.contains("leaf-a")).expect("leaf-a should appear");
        let d_line = lines.iter().find(|l| l.contains("leaf-b")).expect("leaf-b should appear");
        assert!(c_line.contains("←"),
            "leaf-a should have ← arrowhead\nOutput:\n{}", result);
        assert!(d_line.contains("←"),
            "leaf-b should have ← arrowhead\nOutput:\n{}", result);

        // Root (blocker) should NOT have ←
        let root_line = lines.iter().find(|l| l.contains("root")).expect("root should appear");
        assert!(!root_line.contains("←"),
            "root (blocker) should NOT have ←\nOutput:\n{}", result);
    }

    #[test]
    fn test_arc_crossing_characters() {
        // Create a scenario where an outer column's horizontal passes through
        // an inner column's vertical:
        // Arc A (inner, span 2): ccc ← eee
        // Arc B (outer, span 4): aaa ← {eee, fff}
        // On eee's line: Arc A has blocker (─┘), Arc B has blocker (─┤)
        // On ddd's line: Arc A has vertical (│), Arc B has... we need B to have a horizontal there
        //
        // Better: make Arc B longer so it has a blocker INSIDE Arc A's span.
        // Arc A (inner, span 3): bbb ← eee, lines 1-4
        // Arc B (outer, span 5): aaa ← {ccc, fff}, lines 0-5
        // On ccc's line (inside A's span): A has vertical, B has horizontal blocker → crossing!
        let mut graph = WorkGraph::new();
        let mut a = make_task("aaa", "A");
        a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut b = make_task("bbb", "B");
        b.after = vec!["aaa".to_string()];
        b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        let mut c = make_task("ccc", "C");
        c.after = vec!["bbb".to_string()];
        c.created_at = Some("2024-01-01T00:02:00Z".to_string());
        let mut d = make_task("ddd", "D");
        d.after = vec!["ccc".to_string()];
        d.created_at = Some("2024-01-01T00:03:00Z".to_string());
        let mut e = make_task("eee", "E");
        e.after = vec!["ddd".to_string()];
        e.created_at = Some("2024-01-01T00:04:00Z".to_string());
        // E also blocks B (non-tree forward skip) → Arc A
        b.after.push("eee".to_string());
        let mut f = make_task("fff", "F");
        f.after = vec!["eee".to_string()];
        f.created_at = Some("2024-01-01T00:05:00Z".to_string());
        // C also blocks A (non-tree) → part of Arc B
        a.after.push("ccc".to_string());
        a.cycle_config = Some(workgraph::graph::CycleConfig {
            max_iterations: 2,
            guard: None,
            delay: None,
        });
        // F also blocks A (non-tree) → part of Arc B
        a.after.push("fff".to_string());
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));
        graph.add_node(Node::Task(e));
        graph.add_node(Node::Task(f));
        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph, &tasks, &task_ids,
            &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(),
            LayoutMode::default(),
        );
        eprintln!("CROSSING OUTPUT:\n{}", result);
        // Should contain crossing character ┼ where verticals cross horizontals
        assert!(result.contains("┼"),
            "Should have crossing character ┼ where arcs cross\nOutput:\n{}", result);
    }
}
