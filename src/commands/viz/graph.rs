use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use workgraph::graph::{Status, Task, TokenUsage, WorkGraph, format_token_display};

use super::ascii::visible_len;

/// Generate a 2D spatial graph layout with Unicode box-drawing characters.
///
/// Layout strategy (top-to-bottom):
/// 1. Topological sort assigns each node a layer (depth from roots)
/// 2. Nodes within a layer are ordered to reduce edge crossings
/// 3. Each node is rendered as a box: ┌─┐ │id│ │status│ └─┘
/// 4. Vertical lines connect parent layer to child layer
/// 5. Fan-out uses ┬ splitters, fan-in uses ┴ mergers
#[allow(clippy::too_many_arguments)]
pub fn generate_graph(
    graph: &WorkGraph,
    tasks: &[&Task],
    task_ids: &HashSet<&str>,
    annotations: &HashMap<String, String>,
    live_token_usage: &HashMap<String, TokenUsage>,
    agency_token_usage: &HashMap<String, TokenUsage>,
    context_ids: &HashSet<String>,
) -> String {
    generate_graph_with_overrides(
        graph,
        tasks,
        task_ids,
        annotations,
        &HashMap::new(),
        live_token_usage,
        agency_token_usage,
        context_ids,
    )
}

/// Like generate_graph but allows overriding the displayed status for each task.
/// Used by trace animation to show historical snapshots.
#[allow(clippy::too_many_arguments)]
pub fn generate_graph_with_overrides(
    _graph: &WorkGraph,
    tasks: &[&Task],
    task_ids: &HashSet<&str>,
    annotations: &HashMap<String, String>,
    status_overrides: &HashMap<&str, Status>,
    live_token_usage: &HashMap<String, TokenUsage>,
    agency_token_usage: &HashMap<String, TokenUsage>,
    context_ids: &HashSet<String>,
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
            Status::Waiting | Status::PendingValidation => "\x1b[33m",
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
            Status::Waiting | Status::PendingValidation => "waiting",
        }
    };

    // Assign layers via BFS from roots
    let roots: Vec<&str> = tasks
        .iter()
        .filter(|t| {
            reverse
                .get(t.id.as_str())
                .map(Vec::is_empty)
                .unwrap_or(true)
        })
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
        lines: Vec<String>,       // content lines (no color)
        color_lines: Vec<String>, // content lines (with color)
        width: usize,             // inner width
    }

    let dim = if use_color { "\x1b[2m" } else { "" };

    let mut box_infos: HashMap<&str, BoxInfo> = HashMap::new();
    for task in tasks {
        let id = task.id.as_str();
        let is_context = context_ids.contains(id);
        let display_id = if id.len() > max_id_len {
            format!("{}…", &id[..max_id_len - 1])
        } else {
            id.to_string()
        };
        let effective_status = status_overrides.get(id).copied().unwrap_or(task.status);
        let status = status_label(&effective_status);

        let is_coordinator = super::is_coordinator_task(task);

        // Context nodes: dimmed, reduced detail (just ID and status)
        let (line1, line2) = if is_context {
            (display_id, status.to_string())
        } else {
            let phase = annotations
                .get(id)
                .map(|a| format!(" {}", a))
                .unwrap_or_default();

            let loop_info = if is_coordinator {
                format!(" [turn {}]", task.loop_iteration)
            } else if let Some(ref cfg) = task.cycle_config {
                if cfg.max_iterations > 0 {
                    if cfg.no_converge {
                        format!(" ↺ forced {}/{}", task.loop_iteration, cfg.max_iterations)
                    } else {
                        format!(" ↺ {}/{}", task.loop_iteration, cfg.max_iterations)
                    }
                } else {
                    " ↺".to_string()
                }
            } else if task.loop_iteration > 0 {
                format!(" ↺ {}", task.loop_iteration)
            } else {
                String::new()
            };

            let usage = task
                .token_usage
                .as_ref()
                .or_else(|| live_token_usage.get(id));
            let agency_usage = agency_token_usage.get(id);
            let token_info = format_token_display(usage, agency_usage)
                .map(|s| format!(" · {}", s))
                .unwrap_or_default();

            (
                display_id,
                format!("{}{}{}{}", status, token_info, phase, loop_info),
            )
        };
        let width = line1.len().max(line2.len());

        let color = if is_coordinator && use_color {
            "\x1b[36m" // cyan for coordinator
        } else if is_context {
            dim
        } else {
            status_color(&effective_status)
        };
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
                            && cl == next_layer_idx
                        {
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
                let child_centers: HashSet<usize> = edges.iter().map(|e| e.child_center).collect();
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

fn avg_parent_pos(
    id: &str,
    reverse: &HashMap<&str, Vec<&str>>,
    prev_pos: &HashMap<&str, usize>,
) -> f64 {
    let parents = match reverse.get(id) {
        Some(p) => p,
        None => return f64::MAX,
    };
    let positions: Vec<usize> = parents
        .iter()
        .filter_map(|p| prev_pos.get(p).copied())
        .collect();
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
    use workgraph::graph::{Node, Task};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
        }
    }

    #[test]
    fn test_generate_graph_empty() {
        let graph = WorkGraph::new();
        let tasks: Vec<&Task> = vec![];
        let task_ids: HashSet<&str> = HashSet::new();
        let no_annots = HashMap::new();
        let result = generate_graph(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );
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
        let result = generate_graph(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );

        assert!(
            result.contains("alpha"),
            "Should contain task id:\n{}",
            result
        );
        assert!(
            result.contains("open"),
            "Should contain status:\n{}",
            result
        );
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
        let result = generate_graph(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );

        // Both boxes should appear
        assert!(result.contains('a'), "Should contain 'a':\n{}", result);
        assert!(result.contains('b'), "Should contain 'b':\n{}", result);
        // Connecting line between layers
        assert!(
            result.contains('│'),
            "Should have vertical connector:\n{}",
            result
        );
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
        let result = generate_graph(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );

        // All children should appear
        assert!(result.contains("c1"), "Should contain c1:\n{}", result);
        assert!(result.contains("c2"), "Should contain c2:\n{}", result);
        assert!(result.contains("c3"), "Should contain c3:\n{}", result);
        // Should have horizontal connector for fan-out
        assert!(
            result.contains('┬'),
            "Should have ┬ for fan-out:\n{}",
            result
        );
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
        let result = generate_graph(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );

        // All nodes should be present
        assert!(result.contains('a'), "Should contain a:\n{}", result);
        assert!(result.contains('b'), "Should contain b:\n{}", result);
        assert!(
            result.contains("merge"),
            "Should contain merge:\n{}",
            result
        );
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
        let result = generate_graph(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );

        // All 4 nodes
        assert!(
            result.contains("start"),
            "Should contain start:\n{}",
            result
        );
        assert!(result.contains("left"), "Should contain left:\n{}", result);
        assert!(
            result.contains("right"),
            "Should contain right:\n{}",
            result
        );
        assert!(result.contains("end"), "Should contain end:\n{}", result);
        // 3 layers of boxes
        let box_tops = result.matches('┌').count();
        assert!(
            box_tops >= 4,
            "Should have at least 4 box tops:\n{}",
            result
        );
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
        let result = generate_graph(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );

        assert!(
            result.contains("in-progress"),
            "Should show in-progress status:\n{}",
            result
        );
        assert!(
            result.contains("blocked"),
            "Should show blocked status:\n{}",
            result
        );
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
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });
        src.loop_iteration = 2;
        let mut tgt = make_task("tgt", "Target");
        tgt.after = vec!["src".to_string()];
        graph.add_node(Node::Task(src));
        graph.add_node(Node::Task(tgt));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );

        assert!(
            result.contains("↺"),
            "Should show loop annotation:\n{}",
            result
        );
        assert!(
            result.contains("2/5"),
            "Should show iteration count:\n{}",
            result
        );
    }

    #[test]
    fn test_generate_graph_long_id_truncation() {
        let mut graph = WorkGraph::new();
        let t1 = make_task("very-long-task-id-that-exceeds-limit", "Long ID");
        graph.add_node(Node::Task(t1));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_graph(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            &HashSet::new(),
        );

        // ID should be truncated with ellipsis
        assert!(result.contains('…'), "Should truncate long id:\n{}", result);
        // Full ID should NOT appear
        assert!(
            !result.contains("very-long-task-id-that-exceeds-limit"),
            "Full long ID should not appear:\n{}",
            result
        );
    }

    #[test]
    fn test_generate_graph_format_parsing() {
        use super::super::OutputFormat;
        assert_eq!(
            "graph".parse::<OutputFormat>().unwrap(),
            OutputFormat::Graph
        );
    }
}
