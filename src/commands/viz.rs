use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, Stdio};
use workgraph::format_hours;
use workgraph::graph::{Status, Task, WorkGraph};

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

/// Options for the viz command
pub struct VizOptions {
    pub all: bool,
    pub status: Option<String>,
    pub critical_path: bool,
    pub format: OutputFormat,
    pub output: Option<String>,
    /// Show internal tasks (assign-*, evaluate-*) that are normally hidden
    pub show_internal: bool,
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
        // For assign-X: the parent is X (assign task has no blocked_by from parent,
        //   but parent has blocked_by assign-X)
        // For evaluate-X: the parent is X (evaluate task is blocked_by X)
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

pub fn run(dir: &Path, options: &VizOptions) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    // Determine which tasks to include
    let tasks_to_show: Vec<_> = graph
        .tasks()
        .filter(|t| {
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

            // Default: show only non-done tasks
            t.status != Status::Done
        })
        .collect();

    // Filter out internal tasks (assign-*, evaluate-*) unless --show-internal
    let empty_annotations = HashMap::new();
    let (tasks_to_show, annotations) = if options.show_internal {
        (tasks_to_show, empty_annotations)
    } else {
        filter_internal_tasks(&graph, tasks_to_show, &empty_annotations)
    };

    let task_ids: HashSet<&str> = tasks_to_show.iter().map(|t| t.id.as_str()).collect();

    // Calculate critical path if requested
    let critical_path_set: HashSet<String> = if options.critical_path {
        calculate_critical_path(&graph, &task_ids)
    } else {
        HashSet::new()
    };

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
        OutputFormat::Ascii => generate_ascii(&graph, &tasks_to_show, &task_ids, &annotations),
        OutputFormat::Graph => generate_graph(&graph, &tasks_to_show, &task_ids, &annotations),
    };

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
        for blocked_by in &task.blocked_by {
            // Only show edge if the blocker is also in our task set
            if task_ids.contains(blocked_by.as_str()) {
                // Check if this edge is on critical path
                let edge_style =
                    if critical_path.contains(&task.id) && critical_path.contains(blocked_by) {
                        "color=red, penwidth=2"
                    } else {
                        ""
                    };

                if edge_style.is_empty() {
                    lines.push(format!(
                        "  \"{}\" -> \"{}\" [label=\"blocks\"];",
                        blocked_by, task.id
                    ));
                } else {
                    lines.push(format!(
                        "  \"{}\" -> \"{}\" [label=\"blocks\", {}];",
                        blocked_by, task.id, edge_style
                    ));
                }
            }
        }

        if let Some(ref assigned) = task.assigned {
            lines.push(format!(
                "  \"{}\" -> \"{}\" [style=dashed, label=\"assigned\"];",
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

        // Loop edges (loops_to) — dashed magenta with iteration info
        for loop_edge in &task.loops_to {
            if task_ids.contains(loop_edge.target.as_str()) {
                let label = format!(
                    "loop {}/{}",
                    graph
                        .get_task(&loop_edge.target)
                        .map(|t| t.loop_iteration)
                        .unwrap_or(0),
                    loop_edge.max_iterations
                );
                lines.push(format!(
                    "  \"{}\" -> \"{}\" [style=dashed, color=magenta, fontcolor=magenta, label=\"{}\"];",
                    task.id, loop_edge.target, label
                ));
            }
        }
    }

    lines.push("}".to_string());

    lines.join("\n")
}

fn generate_mermaid(
    graph: &WorkGraph,
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
        for blocked_by in &task.blocked_by {
            if task_ids.contains(blocked_by.as_str()) {
                // Check if this edge is on critical path
                let arrow =
                    if critical_path.contains(&task.id) && critical_path.contains(blocked_by) {
                        "==>" // thick arrow for critical path
                    } else {
                        "-->"
                    };

                lines.push(format!("  {} {} {}", blocked_by, arrow, task.id));
            }
        }
    }

    // Print loop edges (loops_to) — dashed magenta
    let mut has_loops = false;
    for task in tasks {
        for loop_edge in &task.loops_to {
            if task_ids.contains(loop_edge.target.as_str()) {
                if !has_loops {
                    lines.push(String::new());
                    lines.push("  %% Loop edges".to_string());
                    has_loops = true;
                }
                let iter_count = graph
                    .get_task(&loop_edge.target)
                    .map(|t| t.loop_iteration)
                    .unwrap_or(0);
                lines.push(format!(
                    "  {} -. \"loop {}/{}\" .-> {}",
                    task.id, iter_count, loop_edge.max_iterations, loop_edge.target
                ));
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

    // Add loop edge styling
    if has_loops {
        lines.push(String::new());
        lines.push("  %% Loop edge styling".to_string());
        lines.push("  linkStyle default stroke:#ff00ff,stroke-dasharray: 5 5".to_string());
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

        for blocker_id in &task.blocked_by {
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
            t.blocked_by
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

/// Generate an ASCII visualization that shows the dependency graph
/// as a proper tree with indentation and branching characters.
///
/// Layout strategy:
/// - Find root nodes (no parents in active set) and DFS from each
/// - Tree structure with ├─→, └─→, │ box-drawing chars
/// - Status shown in parens after each task ID
/// - Fan-out: siblings shown with ├/└ branching
/// - Fan-in: when a node has multiple parents, show it under its first
///   parent and annotate with "(also ← parent2, parent3)"
/// - Connected components grouped together, separated by blank lines
/// - Independent tasks listed at bottom
/// - Color coding by status via ANSI escape codes
fn generate_ascii(
    graph: &WorkGraph,
    tasks: &[&workgraph::graph::Task],
    task_ids: &HashSet<&str>,
    annotations: &HashMap<String, String>,
) -> String {
    if tasks.is_empty() {
        return String::from("(no tasks to display)");
    }

    // Build adjacency within the active set
    // forward: parent → children (parent blocks children)
    // reverse: child → parents (child is blocked by parents)
    let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in tasks {
        for blocker in &task.blocked_by {
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
    // Sort adjacency lists for deterministic output
    for v in forward.values_mut() {
        v.sort();
    }
    for v in reverse.values_mut() {
        v.sort();
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
            .filter(|t| !t.loops_to.is_empty() || t.loop_iteration > 0)
            .map(|t| {
                // Show iteration progress: pick the max iteration / max from loops_to,
                // or use loop_iteration directly if this task is a loop target.
                let (iter, max) = if !t.loops_to.is_empty() {
                    // Task has outgoing loop edges — show target iteration info
                    let edge = &t.loops_to[0];
                    let iter = graph
                        .get_task(&edge.target)
                        .map(|tgt| tgt.loop_iteration)
                        .unwrap_or(0);
                    (iter, edge.max_iterations)
                } else {
                    // Task is a loop target (has loop_iteration > 0 but no outgoing loops)
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
        format!(
            "{}{}{}  ({}){}{}",
            color, id, reset, status, phase_info, loop_info
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
        for blocker in &task.blocked_by {
            if let Some(&bi) = id_to_idx.get(blocker.as_str()) {
                union(&mut parent_uf, ti, bi);
            }
        }
    }

    // Group tasks by component
    let mut components: HashMap<usize, Vec<&str>> = HashMap::new();
    for &id in &all_ids {
        if is_independent(id) {
            continue; // handle independently at the bottom
        }
        let root = find(&mut parent_uf, id_to_idx[id]);
        components.entry(root).or_default().push(id);
    }

    // For each component, find roots (tasks with no parents in active set)
    // and perform DFS tree traversal
    let mut lines: Vec<String> = Vec::new();
    let mut rendered: HashSet<&str> = HashSet::new();

    // Sort components deterministically by their first root's name
    let mut component_list: Vec<Vec<&str>> = components.into_values().collect();
    component_list.retain(|c| !c.is_empty());
    component_list.sort_by(|a, b| {
        let a_min = a.iter().min().unwrap_or(&"");
        let b_min = b.iter().min().unwrap_or(&"");
        a_min.cmp(b_min)
    });

    for component in &component_list {
        // Find roots in this component (no parents in active set)
        let mut roots: Vec<&str> = component
            .iter()
            .filter(|&&id| reverse.get(id).map(Vec::is_empty).unwrap_or(true))
            .copied()
            .collect();
        roots.sort();

        // If no roots found (cycle), pick the alphabetically first
        if roots.is_empty() {
            let mut sorted = component.clone();
            sorted.sort();
            roots.push(sorted[0]);
        }

        if !lines.is_empty() {
            lines.push(String::new()); // blank line between components
        }

        // DFS from each root
        for root in &roots {
            #[allow(clippy::too_many_arguments)]
            fn render_tree<'a>(
                id: &'a str,
                prefix: &str,
                is_last: bool,
                is_root: bool,
                lines: &mut Vec<String>,
                rendered: &mut HashSet<&'a str>,
                forward: &HashMap<&str, Vec<&'a str>>,
                reverse: &HashMap<&str, Vec<&'a str>>,
                format_node: &dyn Fn(&str) -> String,
                task_map: &HashMap<&str, &workgraph::graph::Task>,
                graph: &WorkGraph,
                use_color: bool,
            ) {
                // Build the connector for this node
                let connector = if is_root {
                    String::new()
                } else if is_last {
                    "└→ ".to_string()
                } else {
                    "├→ ".to_string()
                };

                // Check if already rendered (fan-in case)
                if rendered.contains(id) {
                    // Show a back-reference
                    lines.push(format!("{}{}{} ...", prefix, connector, format_node(id)));
                    return;
                }

                rendered.insert(id);

                // Check for additional parents (fan-in annotation)
                let parents = reverse.get(id).map(Vec::as_slice).unwrap_or(&[]);
                let fan_in_note = if parents.len() > 1 {
                    // We're being shown under one parent; note the others
                    let others: Vec<&str> = parents.to_vec();
                    format!("  (← {})", others.join(", "))
                } else {
                    String::new()
                };

                let node_str = format_node(id);
                lines.push(format!(
                    "{}{}{}{}",
                    prefix, connector, node_str, fan_in_note
                ));

                // Compute child prefix
                let child_prefix = if is_root {
                    prefix.to_string()
                } else if is_last {
                    format!("{}  ", prefix)
                } else {
                    format!("{}│ ", prefix)
                };

                // Draw loop back-edges if this task has any
                if let Some(task) = task_map.get(id) {
                    let magenta = if use_color { "\x1b[35m" } else { "" };
                    let reset = if use_color { "\x1b[0m" } else { "" };
                    for loop_edge in &task.loops_to {
                        let iter = graph
                            .get_task(&loop_edge.target)
                            .map(|t| t.loop_iteration)
                            .unwrap_or(0);
                        lines.push(format!(
                            "{}{}↺ loops to {} (iter {}/{})",
                            child_prefix, magenta, loop_edge.target, iter, loop_edge.max_iterations
                        ));
                        if use_color {
                            // Append reset to the last line
                            if let Some(last) = lines.last_mut() {
                                last.push_str(reset);
                            }
                        }
                    }
                }

                // Get children and recurse
                let children = forward.get(id).map(Vec::as_slice).unwrap_or(&[]);
                let child_count = children.len();
                for (i, &child) in children.iter().enumerate() {
                    let child_is_last = i == child_count - 1;
                    render_tree(
                        child,
                        &child_prefix,
                        child_is_last,
                        false,
                        lines,
                        rendered,
                        forward,
                        reverse,
                        format_node,
                        task_map,
                        graph,
                        use_color,
                    );
                }
            }

            render_tree(
                root,
                "",
                true,
                true,
                &mut lines,
                &mut rendered,
                &forward,
                &reverse,
                &format_node,
                &task_map,
                graph,
                use_color,
            );
        }
    }

    // Render independent tasks (no edges in active set)
    let mut independents: Vec<&str> = tasks
        .iter()
        .filter(|t| is_independent(t.id.as_str()))
        .map(|t| t.id.as_str())
        .collect();
    independents.sort();

    if !independents.is_empty() {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        for id in independents {
            lines.push(format!("{}  (independent)", format_node(id)));
        }
    }

    lines.join("\n")
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
) -> String {
    generate_graph_with_overrides(graph, tasks, task_ids, annotations, &HashMap::new())
}

/// Like generate_graph but allows overriding the displayed status for each task.
/// Used by trace animation to show historical snapshots.
pub fn generate_graph_with_overrides(
    graph: &WorkGraph,
    tasks: &[&Task],
    task_ids: &HashSet<&str>,
    annotations: &HashMap<String, String>,
    status_overrides: &HashMap<&str, Status>,
) -> String {
    if tasks.is_empty() {
        return String::from("(no tasks to display)");
    }

    // Build adjacency
    let mut forward: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
    for task in tasks {
        for blocker in &task.blocked_by {
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

        let loop_info = if !task.loops_to.is_empty() || task.loop_iteration > 0 {
            " ↺"
        } else {
            ""
        };

        let line1 = display_id;
        let line2 = format!("{}{}{}", status, phase, loop_info);
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

        for ci in 0..num_content_lines {
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
                    content_rows[ci]
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
                        if let Some(&(cl, cn)) = node_pos.get(cid) {
                            if cl == next_layer_idx {
                                let cc = box_center(cl, cn);
                                edges.push(Edge {
                                    parent_center: pc,
                                    child_center: cc,
                                });
                            }
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

    // Append loop edge annotations at the bottom
    let mut loop_lines: Vec<String> = Vec::new();
    for task in tasks {
        for loop_edge in &task.loops_to {
            if task_ids.contains(loop_edge.target.as_str()) {
                let iter = graph
                    .get_task(&loop_edge.target)
                    .map(|t| t.loop_iteration)
                    .unwrap_or(0);
                let magenta = if use_color { "\x1b[35m" } else { "" };
                loop_lines.push(format!(
                    "{}↺ {} loops to {} (iter {}/{}){}",
                    magenta, task.id, loop_edge.target, iter, loop_edge.max_iterations, reset
                ));
            }
        }
    }
    if !loop_lines.is_empty() {
        output.push(String::new());
        output.extend(loop_lines);
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
            description: None,
            status: Status::Open,
            assigned: None,
            estimate: Some(Estimate {
                hours: Some(hours),
                cost: None,
            }),
            blocks: vec![],
            blocked_by: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
            paused: false,
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
        t2.blocked_by = vec!["t1".to_string()];

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
        t2.blocked_by = vec!["t1".to_string()];

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
        t2.blocked_by = vec!["t1".to_string()];

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
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task_with_hours("t3", "Task 3", 2.0);
        t3.blocked_by = vec!["t1".to_string()];

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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots);
        assert_eq!(result, "(no tasks to display)");
    }

    #[test]
    fn test_generate_ascii_simple_edge() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("src", "Source task");
        let mut t2 = make_task("tgt", "Target task");
        t2.blocked_by = vec!["src".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots);

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
        t2.blocked_by = vec!["a".to_string()];
        let mut t3 = make_task("c", "Task C");
        t3.blocked_by = vec!["a".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots);

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
        t3.blocked_by = vec!["a".to_string(), "b".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots);

        // c should appear under one parent with a fan-in annotation
        assert!(result.contains('c'));
        assert!(result.contains("(←"));
    }

    #[test]
    fn test_generate_ascii_independent() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("solo", "Solo task");
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots);

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
        t2.blocked_by = vec!["root".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots);

        assert!(result.contains("(in-progress)"));
        assert!(result.contains("(blocked)"));
    }

    #[test]
    fn test_generate_ascii_chain() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("a", "Task A");
        let mut t2 = make_task("b", "Task B");
        t2.blocked_by = vec!["a".to_string()];
        let mut t3 = make_task("c", "Task C");
        t3.blocked_by = vec!["b".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots);

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
        t2.blocked_by = vec!["t1".to_string()];

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

    fn make_internal_task(id: &str, title: &str, tag: &str, blocked_by: Vec<&str>) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            tags: vec![tag.to_string(), "agency".to_string()],
            blocked_by: blocked_by.into_iter().map(String::from).collect(),
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
        parent.blocked_by = vec!["assign-my-task".to_string()];
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        let annotations = HashMap::new();
        let (filtered, annots) =
            filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        let result = generate_ascii(&graph, &filtered, &task_ids, &annots);

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

        let result = generate_ascii(&graph, &filtered, &task_ids, &annots);

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
        parent.blocked_by = vec!["assign-my-task".to_string()];
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
        parent.blocked_by = vec!["assign-my-task".to_string()];
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
        parent.blocked_by = vec!["assign-my-task".to_string()];
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        // When show_internal is true, we skip filtering
        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let annots = HashMap::new();

        let result = generate_ascii(&graph, &tasks, &task_ids, &annots);

        // Both tasks should be visible
        assert!(result.contains("assign-my-task"));
        assert!(result.contains("my-task"));
        // No phase annotation when shown as literal nodes
        assert!(!result.contains("[assigning]"));
    }

    #[test]
    fn test_ascii_loop_symbol_on_task_with_loops_to() {
        use workgraph::graph::LoopEdge;

        let mut graph = WorkGraph::new();
        let mut src = make_task("src", "Source");
        src.loops_to.push(LoopEdge {
            target: "tgt".to_string(),
            guard: None,
            max_iterations: 10,
            delay: None,
        });
        let mut tgt = make_task("tgt", "Target");
        tgt.loop_iteration = 3;
        tgt.blocked_by = vec!["src".to_string()];
        // Need src blocked_by tgt for the loop chain
        src.blocked_by = vec!["tgt".to_string()];
        graph.add_node(Node::Task(src));
        graph.add_node(Node::Task(tgt));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots);

        // The source task (which has loops_to) should show the ↺ symbol
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
        use workgraph::graph::LoopEdge;

        let mut graph = WorkGraph::new();
        let mut task = make_task("looper", "Looping task");
        task.loops_to.push(LoopEdge {
            target: "looper".to_string(),
            guard: None,
            max_iterations: 5,
            delay: None,
        });
        task.loop_iteration = 2;
        graph.add_node(Node::Task(task));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots);

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
    fn test_ascii_loop_backedge_uses_loop_symbol() {
        use workgraph::graph::LoopEdge;

        let mut graph = WorkGraph::new();
        let mut src = make_task("src", "Source");
        src.loops_to.push(LoopEdge {
            target: "tgt".to_string(),
            guard: None,
            max_iterations: 5,
            delay: None,
        });
        let mut tgt = make_task("tgt", "Target");
        tgt.blocked_by = vec!["src".to_string()];
        graph.add_node(Node::Task(src));
        graph.add_node(Node::Task(tgt));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots);

        // The loop back-edge line should contain ↺
        assert!(
            result.contains("↺"),
            "Expected ↺ symbol in back-edge line:\n{}",
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots);

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
        task_b.blocked_by = vec!["assign-b".to_string()];
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
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots);
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
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots);

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
        t2.blocked_by = vec!["a".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots);

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
        c1.blocked_by = vec!["root".to_string()];
        let mut c2 = make_task("c2", "Child 2");
        c2.blocked_by = vec!["root".to_string()];
        let mut c3 = make_task("c3", "Child 3");
        c3.blocked_by = vec!["root".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(c1));
        graph.add_node(Node::Task(c2));
        graph.add_node(Node::Task(c3));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots);

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
        merge.blocked_by = vec!["a".to_string(), "b".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(merge));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots);

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
        left.blocked_by = vec!["start".to_string()];
        let mut right = make_task("right", "Right");
        right.blocked_by = vec!["start".to_string()];
        let mut end = make_task("end", "End");
        end.blocked_by = vec!["left".to_string(), "right".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(left));
        graph.add_node(Node::Task(right));
        graph.add_node(Node::Task(end));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots);

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
        t2.blocked_by = vec!["root".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots);

        assert!(result.contains("in-progress"), "Should show in-progress status:\n{}", result);
        assert!(result.contains("blocked"), "Should show blocked status:\n{}", result);
    }

    #[test]
    fn test_generate_graph_loop_annotation() {
        use workgraph::graph::LoopEdge;

        let mut graph = WorkGraph::new();
        let mut src = make_task("src", "Source");
        src.loops_to.push(LoopEdge {
            target: "tgt".to_string(),
            guard: None,
            max_iterations: 5,
            delay: None,
        });
        let mut tgt = make_task("tgt", "Target");
        tgt.loop_iteration = 2;
        tgt.blocked_by = vec!["src".to_string()];
        graph.add_node(Node::Task(src));
        graph.add_node(Node::Task(tgt));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots);

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
        let result = generate_graph(&graph, &tasks, &task_ids, &no_annots);

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
}
