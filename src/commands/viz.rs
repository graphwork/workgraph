use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, Stdio};
use workgraph::graph::{Status, WorkGraph};
use workgraph::parser::load_graph;

use super::graph_path;

/// Output format for visualization
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Dot,
    Mermaid,
    Ascii,
}

impl std::str::FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "dot" => Ok(OutputFormat::Dot),
            "mermaid" => Ok(OutputFormat::Mermaid),
            "ascii" | "dag" => Ok(OutputFormat::Ascii),
            _ => Err(format!(
                "Unknown format: {}. Use 'dot', 'mermaid', or 'ascii'.",
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
}

impl Default for VizOptions {
    fn default() -> Self {
        Self {
            all: false,
            status: None,
            critical_path: false,
            format: OutputFormat::Dot,
            output: None,
        }
    }
}

pub fn run(dir: &Path, options: VizOptions) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

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
                    Status::PendingReview => "pending-review",
                };
                return task_status == status_filter.to_lowercase();
            }

            // Default: show only non-done tasks
            t.status != Status::Done
        })
        .collect();

    let task_ids: HashSet<&str> = tasks_to_show.iter().map(|t| t.id.as_str()).collect();

    // Calculate critical path if requested
    let critical_path_set: HashSet<String> = if options.critical_path {
        calculate_critical_path(&graph, &task_ids)
    } else {
        HashSet::new()
    };

    // Generate output
    let output = match options.format {
        OutputFormat::Dot => generate_dot(&graph, &tasks_to_show, &task_ids, &critical_path_set),
        OutputFormat::Mermaid => {
            generate_mermaid(&tasks_to_show, &task_ids, &critical_path_set)
        }
        OutputFormat::Ascii => {
            generate_ascii(&graph, &tasks_to_show, &task_ids)
        }
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
) -> String {
    let mut lines = Vec::new();

    lines.push("digraph workgraph {".to_string());
    lines.push("  rankdir=LR;".to_string());
    lines.push("  node [shape=box];".to_string());
    lines.push(String::new());

    // Print task nodes
    for task in tasks {
        let style = match task.status {
            Status::Done => "style=filled, fillcolor=lightgreen",
            Status::InProgress => "style=filled, fillcolor=lightyellow",
            Status::Blocked => "style=filled, fillcolor=lightcoral",
            Status::Open => "style=filled, fillcolor=white",
            Status::Failed => "style=filled, fillcolor=salmon",
            Status::Abandoned => "style=filled, fillcolor=lightgray",
            Status::PendingReview => "style=filled, fillcolor=lightskyblue",
        };

        // Build label with hours estimate if available
        let hours_str = task
            .estimate
            .as_ref()
            .and_then(|e| e.hours)
            .map(|h| format!("\\n{}h", format_hours(h)))
            .unwrap_or_default();

        let label = format!("{}\\n{}{}", task.id, task.title, hours_str);

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
    let assigned_actors: HashSet<&str> = tasks
        .iter()
        .filter_map(|t| t.assigned.as_deref())
        .collect();

    for actor_id in &assigned_actors {
        lines.push(format!(
            "  \"{}\" [label=\"{}\", shape=ellipse, style=filled, fillcolor=lightblue];",
            actor_id, actor_id
        ));
    }

    // Print resources that are required by shown tasks
    let required_resources: HashSet<&str> = tasks
        .iter()
        .flat_map(|t| t.requires.iter().map(|s| s.as_str()))
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
    }

    lines.push("}".to_string());

    lines.join("\n")
}

fn generate_mermaid(
    tasks: &[&workgraph::graph::Task],
    task_ids: &HashSet<&str>,
    critical_path: &HashSet<String>,
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
        let label = format!("{}: {}{}", task.id, title, hours_str);

        // Mermaid node shape based on status
        let node = match task.status {
            Status::Done => format!("  {}[/\"{}\"/]", task.id, label),
            Status::InProgress => format!("  {}((\"{}\"))", task.id, label),
            Status::Blocked => format!("  {}{{\"{}\"}}!", task.id, label),
            Status::Open => format!("  {}[\"{}\"]", task.id, label),
            Status::Failed => format!("  {}{{{{\"{}\"}}}}!", task.id, label),
            Status::Abandoned => format!("  {}[\"{}\"]:::abandoned", task.id, label),
            Status::PendingReview => format!("  {}([\"{}\"]) ", task.id, label), // Stadium shape
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

    // Print actor assignments
    let assigned_actors: HashSet<&str> = tasks
        .iter()
        .filter_map(|t| t.assigned.as_deref())
        .collect();

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
        let critical_nodes: Vec<&str> = critical_path.iter().map(|s| s.as_str()).collect();
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
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
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
                .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
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

/// Generate an ASCII DAG visualization that shows the dependency graph
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
    _graph: &WorkGraph,
    tasks: &[&workgraph::graph::Task],
    task_ids: &HashSet<&str>,
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
            Status::Done => "\x1b[32m",          // green
            Status::InProgress => "\x1b[33m",    // yellow
            Status::Open => "\x1b[37m",          // white
            Status::Blocked => "\x1b[90m",       // gray
            Status::Failed => "\x1b[31m",        // red
            Status::Abandoned => "\x1b[90m",     // gray
            Status::PendingReview => "\x1b[36m", // cyan
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
            Status::PendingReview => "pending-review",
        }
    };

    let format_node = |id: &str| -> String {
        let task = task_map.get(id);
        let color = task.map(|t| status_color(&t.status)).unwrap_or("");
        let status = task.map(|t| status_label(&t.status)).unwrap_or("unknown");
        format!("{}{}{}  ({})", color, id, reset, status)
    };

    // Find connected components using union-find
    let all_ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let id_to_idx: HashMap<&str, usize> = all_ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();
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
    component_list.sort_by(|a, b| {
        let a_min = a.iter().min().unwrap();
        let b_min = b.iter().min().unwrap();
        a_min.cmp(b_min)
    });

    for component in &component_list {
        // Find roots in this component (no parents in active set)
        let mut roots: Vec<&str> = component
            .iter()
            .filter(|&&id| {
                reverse.get(id).map(|p| p.is_empty()).unwrap_or(true)
            })
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
            ) {
                // Build the connector for this node
                let connector = if is_root {
                    String::new()
                } else if is_last {
                    "└─→ ".to_string()
                } else {
                    "├─→ ".to_string()
                };

                // Check if already rendered (fan-in case)
                if rendered.contains(id) {
                    // Show a back-reference
                    lines.push(format!("{}{}{} ...", prefix, connector, format_node(id)));
                    return;
                }

                rendered.insert(id);

                // Check for additional parents (fan-in annotation)
                let parents = reverse.get(id).map(|v| v.as_slice()).unwrap_or(&[]);
                let fan_in_note = if parents.len() > 1 {
                    // We're being shown under one parent; note the others
                    let others: Vec<&str> = parents.iter().copied().collect();
                    format!("  (← {})", others.join(", "))
                } else {
                    String::new()
                };

                let node_str = format_node(id);
                lines.push(format!("{}{}{}{}", prefix, connector, node_str, fan_in_note));

                // Compute child prefix
                let child_prefix = if is_root {
                    prefix.to_string()
                } else if is_last {
                    format!("{}      ", prefix)
                } else {
                    format!("{}│     ", prefix)
                };

                // Get children and recurse
                let children = forward.get(id).map(|v| v.as_slice()).unwrap_or(&[]);
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

/// Format hours nicely (no decimals if whole number)
fn format_hours(hours: f64) -> String {
    if hours.fract() == 0.0 {
        format!("{}", hours as i64)
    } else {
        format!("{:.1}", hours)
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
            description: None,
            status: Status::Open,
            assigned: None,
            estimate: None,
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

        let dot = generate_dot(&graph, &tasks, &task_ids, &critical_path);
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

        let dot = generate_dot(&graph, &tasks, &task_ids, &critical_path);
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

        let dot = generate_dot(&graph, &tasks, &task_ids, &critical_path);
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

        let mermaid = generate_mermaid(&tasks, &task_ids, &critical_path);
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

        let mermaid = generate_mermaid(&tasks, &task_ids, &critical_path);
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
        assert_eq!(
            "dag".parse::<OutputFormat>().unwrap(),
            OutputFormat::Ascii
        );
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
        let result = generate_ascii(&graph, &tasks, &task_ids);
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
        let result = generate_ascii(&graph, &tasks, &task_ids);

        // Tree output: src is root, tgt is child
        assert!(result.contains("src"));
        assert!(result.contains("tgt"));
        assert!(result.contains("└─→"));
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
        let result = generate_ascii(&graph, &tasks, &task_ids);

        // a is root with two children
        assert!(result.contains("├─→"));
        assert!(result.contains("└─→"));
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
        let result = generate_ascii(&graph, &tasks, &task_ids);

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
        let result = generate_ascii(&graph, &tasks, &task_ids);

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
        let result = generate_ascii(&graph, &tasks, &task_ids);

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
        let result = generate_ascii(&graph, &tasks, &task_ids);

        // Should show indented chain: a -> b -> c
        assert!(result.contains("a"));
        assert!(result.contains("b"));
        assert!(result.contains("c"));
        // b and c should be indented (have └─→ prefix)
        let result_lines: Vec<&str> = result.lines().collect();
        // First line is the root (a), no prefix
        assert!(result_lines[0].contains("a"));
        // Nested nodes should have tree characters
        assert!(result.contains("└─→"));
    }
}
