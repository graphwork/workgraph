use std::collections::{HashMap, HashSet, VecDeque};
use std::io::IsTerminal;
use workgraph::graph::{format_token_display, Status, Task, TokenUsage, WorkGraph};

use super::{LayoutMode, VizOutput};

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
#[allow(clippy::only_used_in_recursion, clippy::too_many_arguments)]
pub(crate) fn generate_ascii(
    graph: &WorkGraph,
    tasks: &[&Task],
    task_ids: &HashSet<&str>,
    annotations: &HashMap<String, String>,
    live_token_usage: &HashMap<String, TokenUsage>,
    assign_token_usage: &HashMap<String, TokenUsage>,
    eval_token_usage: &HashMap<String, TokenUsage>,
    layout: LayoutMode,
    context_ids: &HashSet<String>,
    edge_color: &str,
) -> VizOutput {
    if tasks.is_empty() {
        return VizOutput {
            text: String::from("(no tasks to display)"),
            node_line_map: HashMap::new(),
            task_order: Vec::new(),
            forward_edges: HashMap::new(),
            reverse_edges: HashMap::new(),
        };
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
    let mut diamond_fan_in_nodes: HashSet<&str> = HashSet::new(); // nodes restructured by diamond layout
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
            let mut zero_in: Vec<&str> = remaining_in
                .iter()
                .filter(|&(_, &deg)| deg == 0)
                .map(|(&id, _)| id)
                .collect();
            zero_in.sort();
            for id in zero_in {
                queue.push_back(id);
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

                if common.is_empty() {
                    continue;
                }

                // Pick the deepest common ancestor (max topo_depth, break ties alphabetically)
                let lca = *common
                    .iter()
                    .max_by(|&&a, &&b| {
                        topo_depth
                            .get(a)
                            .unwrap_or(&0)
                            .cmp(topo_depth.get(b).unwrap_or(&0))
                            .then_with(|| a.cmp(b))
                    })
                    .unwrap();

                // Track this as a restructured fan-in node
                diamond_fan_in_nodes.insert(fan_in);

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

    // Re-sort forward lists after diamond restructuring to restore deterministic order.
    // Fan-in nodes (moved to LCA) must stay after their sibling-parents, so sort in two tiers:
    // non-fan-in children alphabetically first, then fan-in children alphabetically.
    for v in forward.values_mut() {
        v.sort_by(|a, b| {
            let a_is_fan_in = diamond_fan_in_nodes.contains(a);
            let b_is_fan_in = diamond_fan_in_nodes.contains(b);
            match (a_is_fan_in, b_is_fan_in) {
                (false, true) => std::cmp::Ordering::Less,
                (true, false) => std::cmp::Ordering::Greater,
                _ => a.cmp(b),
            }
        });
    }

    // Task lookup
    let task_map: HashMap<&str, &Task> =
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

    let dim = if use_color { "\x1b[2m" } else { "" };

    // Edge color: tree connectors (├→ └→ │) and arc lines (←┐ ┘ │ ─)
    // "gray" = both gray, "white" = both white, "mixed" = tree default + arcs gray
    let (tree_color, arc_color) = if use_color {
        match edge_color {
            "white" => ("\x1b[37m", "\x1b[37m"),
            "mixed" => ("", "\x1b[90m"),
            _ => ("\x1b[90m", "\x1b[90m"), // "gray" default
        }
    } else {
        ("", "")
    };

    let format_node = |id: &str| -> String {
        let task = task_map.get(id);
        let is_context = context_ids.contains(id);
        let status = task.map(|t| status_label(&t.status)).unwrap_or("unknown");

        // Context nodes: dimmed, reduced detail (just ID and status, no tokens/phase/loop)
        if is_context {
            return format!(
                "{}{}  ({}){}",
                dim, id, status, reset
            );
        }

        let color = task.map(|t| status_color(&t.status)).unwrap_or("");
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
                task_map: &HashMap<&str, &Task>,
                use_color: bool,
                node_line_map: &mut HashMap<&'a str, usize>,
                back_edge_arcs: &mut Vec<BackEdgeArc>,
                invisible_visits: &HashSet<(String, String)>,
                tree_color: &str,
                color_reset: &str,
            ) {
                let connector = if is_root {
                    String::new()
                } else if is_last {
                    format!("{}└→{} ", tree_color, color_reset)
                } else {
                    format!("{}├→{} ", tree_color, color_reset)
                };

                // Already rendered: record arc, emit nothing
                if rendered.contains(id) {
                    if let Some(pid) = parent_id
                        && let Some(&blocker_line) = node_line_map.get(pid)
                            && let Some(&dependent_line) = node_line_map.get(id) {
                                back_edge_arcs.push(BackEdgeArc {
                                    blocker_line,
                                    dependent_line,
                                });
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
                    format!("{}{}│{} ", prefix, tree_color, color_reset)
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
                        tree_color,
                        color_reset,
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
                tree_color,
                reset,
            );
        }

        // For single-node independent WCCs, append "(independent)" label
        if component.len() == 1 && is_independent(component[0])
            && let Some(last_line) = lines.last_mut() {
                last_line.push_str("  (independent)");
            }
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
    let has_crossings = draw_back_edge_arcs(&mut lines, &back_edge_arcs, use_color, arc_color);

    // Append legend when crossings are present
    if has_crossings {
        lines.push(String::new());
        lines.push(format!(
            "{}Legend: ┼ = crossing (vertical arc passes through horizontal){}", dim, reset
        ));
    }

    // Build owned node_line_map and task_order (sorted by line number)
    let owned_node_line_map: HashMap<String, usize> = node_line_map
        .iter()
        .map(|(&id, &line)| (id.to_string(), line))
        .collect();
    let mut task_order: Vec<(String, usize)> = owned_node_line_map
        .iter()
        .map(|(id, &line)| (id.clone(), line))
        .collect();
    task_order.sort_by_key(|(_, line)| *line);
    let task_order: Vec<String> = task_order.into_iter().map(|(id, _)| id).collect();

    // Build owned forward/reverse edge maps from the visible task set
    let mut forward_edges: HashMap<String, Vec<String>> = HashMap::new();
    let mut reverse_edges: HashMap<String, Vec<String>> = HashMap::new();
    for task in tasks {
        for blocker in &task.after {
            if task_ids.contains(blocker.as_str()) {
                forward_edges
                    .entry(blocker.clone())
                    .or_default()
                    .push(task.id.clone());
                reverse_edges
                    .entry(task.id.clone())
                    .or_default()
                    .push(blocker.clone());
            }
        }
    }

    VizOutput {
        text: lines.join("\n"),
        node_line_map: owned_node_line_map,
        task_order,
        forward_edges,
        reverse_edges,
    }
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
/// Returns true if any arc crossings (┼) were drawn.
fn draw_back_edge_arcs(lines: &mut [String], arcs: &[BackEdgeArc], use_color: bool, arc_color_code: &str) -> bool {
    if arcs.is_empty() {
        return false;
    }
    let mut has_crossings = false;

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
        return false;
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

    let dim = if use_color { arc_color_code } else { "" };
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
                    } else if is_dep { "←┤" } else { "─┤" };

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
                            has_crossings = true;
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
    has_crossings
}

/// Strip ANSI escape codes to get visible length.
pub(crate) fn visible_len(s: &str) -> usize {
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
    fn test_generate_ascii_empty() {
        let graph = WorkGraph::new();
        let tasks: Vec<&Task> = vec![];
        let task_ids: HashSet<&str> = HashSet::new();
        let no_annots = HashMap::new();
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");
        assert_eq!(result.text, "(no tasks to display)");
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Tree output: src is root, tgt is child
        assert!(result.text.contains("src"));
        assert!(result.text.contains("tgt"));
        assert!(result.text.contains("└→"));
        assert!(result.text.contains("(open)"));
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // a is root with two children
        assert!(result.text.contains("├→"));
        assert!(result.text.contains("└→"));
        assert!(result.text.contains('a'));
        assert!(result.text.contains('b'));
        assert!(result.text.contains('c'));
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // c should appear, and the fan-in edge should be shown as a right-side arc
        assert!(result.text.contains('c'));
        // Fan-in is now shown via right-side arcs (←/┘) instead of text annotations
        assert!(
            result.text.contains("←") || result.text.contains("┘"),
            "Fan-in should produce a right-side arc.\nOutput:\n{}",
            result.text
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        assert!(result.text.contains("solo"));
        assert!(result.text.contains("(independent)"));
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        assert!(result.text.contains("(in-progress)"));
        assert!(result.text.contains("(blocked)"));
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Should show indented chain: a -> b -> c
        assert!(result.text.contains("a"));
        assert!(result.text.contains("b"));
        assert!(result.text.contains("c"));
        // b and c should be indented (have └─→ prefix)
        let result_lines: Vec<&str> = result.text.lines().collect();
        // First line is the root (a), no prefix
        assert!(result_lines[0].contains("a"));
        // Nested nodes should have tree characters
        assert!(result.text.contains("└→"));
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
            crate::commands::viz::filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        let result = generate_ascii(&graph, &filtered, &task_ids, &annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Internal task should NOT appear
        assert!(!result.text.contains("assign-my-task"));
        // Parent task should appear with phase annotation
        assert!(result.text.contains("my-task"));
        assert!(result.text.contains("[assigning]"));
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
            crate::commands::viz::filter_internal_tasks(&graph, graph.tasks().collect(), &annotations);
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        let result = generate_ascii(&graph, &filtered, &task_ids, &annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        assert!(!result.text.contains("evaluate-my-task"));
        assert!(result.text.contains("my-task"));
        assert!(result.text.contains("[evaluating]"));
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

        let result = generate_ascii(&graph, &tasks, &task_ids, &annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Both tasks should be visible
        assert!(result.text.contains("assign-my-task"));
        assert!(result.text.contains("my-task"));
        // No phase annotation when shown as literal nodes
        assert!(!result.text.contains("[assigning]"));
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // The source task (which has cycle_config) should show the ↺ symbol
        assert!(
            result.text.contains("↺"),
            "Expected ↺ symbol in output:\n{}",
            result.text
        );
        // Should show iteration info like (iter 3/10)
        assert!(
            result.text.contains("3/10"),
            "Expected iteration count in output:\n{}",
            result.text
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Should show ↺ symbol in the node label
        assert!(
            result.text.contains("↺"),
            "Expected ↺ symbol in output:\n{}",
            result.text
        );
        assert!(
            result.text.contains("2/5"),
            "Expected iteration count in output:\n{}",
            result.text
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Task with cycle_config should show the ↺ symbol
        assert!(
            result.text.contains("↺"),
            "Expected ↺ symbol for cycle_config task:\n{}",
            result.text
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &no_annots, &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // No loop symbol on tasks without loops
        assert!(
            !result.text.contains("↺"),
            "Should NOT contain ↺ on normal task:\n{}",
            result.text
        );
        assert!(
            !result.text.contains("↻"),
            "Should NOT contain ↻ on normal task:\n{}",
            result.text
        );
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Should have ← at target and ┘ at source
        assert!(result.text.contains("←"), "Back-edge target should have ←\nOutput:\n{}", result.text);
        assert!(result.text.contains("┘"), "Back-edge source should have ┘\nOutput:\n{}", result.text);
        // Should NOT have old-style cycle-back text
        assert!(!result.text.contains("cycles back"), "No old-style text\nOutput:\n{}", result.text);
        // Should NOT have fan-in annotations
        assert!(!result.text.contains("(←"), "No fan-in text annotations\nOutput:\n{}", result.text);
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Fan-in should produce a right-side arc (not a text annotation)
        assert!(result.text.contains("←") || result.text.contains("┘"),
            "Diamond fan-in should have right-side arcs\nOutput:\n{}", result.text);
        assert!(!result.text.contains("(←"), "No fan-in text annotation\nOutput:\n{}", result.text);
        assert!(!result.text.contains("..."), "No duplicate 'already shown' entries\nOutput:\n{}", result.text);
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::Diamond, &HashSet::new(), "gray");
        eprintln!("DIAMOND:\n{}", result.text);

        let lines: Vec<&str> = result.text.lines().collect();
        // join should be at the same indentation as left and right (direct child of root)
        let join_line = lines.iter().find(|l| l.contains("join")).expect("join should appear");
        let left_line = lines.iter().find(|l| l.contains("left")).expect("left should appear");
        // Both should start with tree connectors at the same indent level
        let join_indent = join_line.find("join").unwrap();
        let left_indent = left_line.find("left").unwrap();
        assert_eq!(join_indent, left_indent,
            "Diamond layout: join should be at same indent as left\nOutput:\n{}", result.text);

        // join should appear AFTER both left and right in line order
        let left_idx = lines.iter().position(|l| l.contains("left")).unwrap();
        let right_idx = lines.iter().position(|l| l.contains("right")).unwrap();
        let join_idx = lines.iter().position(|l| l.contains("join")).unwrap();
        assert!(join_idx > left_idx, "join should be after left");
        assert!(join_idx > right_idx, "join should be after right");

        // Arcs should flow DOWN (left and right have ┐ or ─, join has ← or ┘)
        assert!(join_line.contains("←") || join_line.contains("┘"),
            "join should receive arcs\nOutput:\n{}", result.text);

        // Compare with tree layout (old behavior): join should be under left
        let result_tree = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::Tree, &HashSet::new(), "gray");
        eprintln!("TREE:\n{}", result_tree.text);
        let tree_lines: Vec<&str> = result_tree.text.lines().collect();
        let join_tree = tree_lines.iter().find(|l| l.contains("join")).expect("join in tree");
        let left_tree = tree_lines.iter().find(|l| l.contains("left")).expect("left in tree");
        let join_tree_indent = join_tree.find("join").unwrap();
        let left_tree_indent = left_tree.find("left").unwrap();
        // In tree mode, join should be DEEPER than left (a child of left)
        assert!(join_tree_indent > left_tree_indent,
            "Tree layout: join should be deeper than left\nOutput:\n{}", result_tree.text);
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::Diamond, &HashSet::new(), "gray");
        eprintln!("WIDE DIAMOND:\n{}", result.text);

        let lines: Vec<&str> = result.text.lines().collect();
        // join should be after all fan-out children
        let join_idx = lines.iter().position(|l| l.contains("join")).unwrap();
        for name in &names {
            let idx = lines.iter().position(|l| l.contains(name)).unwrap();
            assert!(join_idx > idx, "join should be after {}", name);
        }
        // join should receive arcs
        let join_line = lines.iter().find(|l| l.contains("join")).unwrap();
        assert!(join_line.contains("←") || join_line.contains("┘"),
            "join should receive arcs in wide diamond\nOutput:\n{}", result.text);
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        let tree_lines: Vec<&str> = result.text.lines().collect();
        // The child should use └→ (last visible child), not ├→
        let child_line = tree_lines.iter().find(|l| l.contains("child"));
        assert!(child_line.is_some(), "Child should appear\nOutput:\n{}", result.text);
        assert!(
            child_line.unwrap().contains("└→"),
            "Child should use └→ (no orphaned ├→)\nLine: '{}'\nOutput:\n{}",
            child_line.unwrap(), result.text
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Should have exactly one ← (same-target collapse)
        let target_count = result.text.matches("←").count();
        assert_eq!(target_count, 1,
            "Multiple sources to same target should collapse to 1 column\nOutput:\n{}", result.text);
        // Should have ┤ for intermediate sources and ┘ for the last
        assert!(result.text.contains("┤"), "Intermediate sources should have ┤\nOutput:\n{}", result.text);
        assert!(result.text.contains("┘"), "Last source should have ┘\nOutput:\n{}", result.text);
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Lines with arcs should have space before the dash fill
        for line in result.text.lines() {
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Find the lines
        let lines: Vec<&str> = result.text.lines().collect();
        let b_line = lines.iter().find(|l| l.contains("bbb")).expect("B should appear");
        let c_line = lines.iter().find(|l| l.contains("ccc")).expect("C should appear");

        // C (dependent) should have ← arrowhead (arc from B flows down to C)
        assert!(c_line.contains("←"),
            "Forward skip: dependent C should have ←\nOutput:\n{}", result.text);
        // B (blocker) should have ┐ (top corner of downward arc to C)
        assert!(b_line.contains("┐"),
            "Forward skip: blocker B should have ┐\nOutput:\n{}", result.text);

        // Verify tree layout with LayoutMode::Tree (old behavior)
        let result_tree = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::Tree, &HashSet::new(), "gray");
        let tree_lines: Vec<&str> = result_tree.text.lines().collect();
        let a_line_tree = tree_lines.iter().find(|l| l.contains("aaa")).expect("A should appear in tree mode");
        // In tree mode, A→C forward skip produces an arc with ┐ at A
        assert!(a_line_tree.contains("┐") || a_line_tree.contains("─"),
            "Tree mode: A should participate in arc\nOutput:\n{}", result_tree.text);
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // D should have exactly one ← (same-dependent collapse)
        let arrow_count = result.text.matches("←").count();
        assert_eq!(arrow_count, 1,
            "Mixed direction arcs to same dependent should collapse to 1 column\nOutput:\n{}", result.text);

        // D's line should have ←
        let lines: Vec<&str> = result.text.lines().collect();
        let d_line = lines.iter().find(|l| l.contains("ddd")).expect("D should appear");
        assert!(d_line.contains("←"),
            "Mixed direction: D should have ←\nOutput:\n{}", result.text);
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
        let result = generate_ascii(&graph, &tasks, &task_ids, &HashMap::new(), &HashMap::new(), &HashMap::new(), &HashMap::new(), LayoutMode::default(), &HashSet::new(), "gray");

        // Each dependent (leaf-a, leaf-b) should have ←
        let lines: Vec<&str> = result.text.lines().collect();
        let c_line = lines.iter().find(|l| l.contains("leaf-a")).expect("leaf-a should appear");
        let d_line = lines.iter().find(|l| l.contains("leaf-b")).expect("leaf-b should appear");
        assert!(c_line.contains("←"),
            "leaf-a should have ← arrowhead\nOutput:\n{}", result.text);
        assert!(d_line.contains("←"),
            "leaf-b should have ← arrowhead\nOutput:\n{}", result.text);

        // Root (blocker) should NOT have ←
        let root_line = lines.iter().find(|l| l.contains("root")).expect("root should appear");
        assert!(!root_line.contains("←"),
            "root (blocker) should NOT have ←\nOutput:\n{}", result.text);
    }

    #[test]
    fn test_arc_crossing_characters() {
        // Create a scenario where an outer column's horizontal passes through
        // an inner column's vertical
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
            LayoutMode::default(), &HashSet::new(), "gray",
        );
        eprintln!("CROSSING OUTPUT:\n{}", result.text);
        // Should contain crossing character ┼ where verticals cross horizontals
        assert!(result.text.contains("┼"),
            "Should have crossing character ┼ where arcs cross\nOutput:\n{}", result.text);
        // Should contain legend explaining the crossing symbol
        assert!(result.text.contains("Legend:"),
            "Should have legend when crossings exist\nOutput:\n{}", result.text);
    }
}
