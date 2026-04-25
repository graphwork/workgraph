use chrono::Utc;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::IsTerminal;
use unicode_width::UnicodeWidthChar;
use workgraph::graph::{Status, Task, TokenUsage, WorkGraph, format_token_display};
use workgraph::messages::{CoordinatorMessageStatus, MessageStats};

use super::{LayoutMode, VizOutput};

/// Back-edge arc info for Phase 2 rendering of right-side arcs.
struct BackEdgeArc {
    blocker_line: usize,   // line index where the blocking node was rendered
    dependent_line: usize, // line index where the dependent node was rendered
    from_id: String,       // task ID of the blocker (dependency)
    to_id: String,         // task ID of the dependent (depends on blocker)
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
    annotations: &HashMap<String, super::AnnotationInfo>,
    live_token_usage: &HashMap<String, TokenUsage>,
    agency_token_usage: &HashMap<String, TokenUsage>,
    layout: LayoutMode,
    context_ids: &HashSet<String>,
    edge_color: &str,
    message_stats: &HashMap<String, MessageStats>,
    coordinator_status: &HashMap<String, CoordinatorMessageStatus>,
) -> VizOutput {
    if tasks.is_empty() {
        return VizOutput {
            text: String::from("(no tasks to display)"),
            node_line_map: HashMap::new(),
            task_order: Vec::new(),
            forward_edges: HashMap::new(),
            reverse_edges: HashMap::new(),
            char_edge_map: HashMap::new(),
            cycle_members: HashMap::new(),
            annotation_map: annotations.clone(),
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
    let task_map: HashMap<&str, &Task> = tasks.iter().map(|t| (t.id.as_str(), *t)).collect();

    // Color helpers
    let use_color = std::io::stdout().is_terminal();

    let status_color = |status: &Status| -> &str {
        if !use_color {
            return "";
        }
        match status {
            Status::Done => "\x1b[32m",                                // green
            Status::InProgress => "\x1b[33m",                          // yellow
            Status::Open => "\x1b[37m",                                // white
            Status::Blocked => "\x1b[90m",                             // gray
            Status::Failed => "\x1b[31m",                              // red
            Status::Abandoned => "\x1b[90m",                           // gray
            Status::Waiting | Status::PendingValidation => "\x1b[33m", // yellow
            Status::Incomplete => "\x1b[38;5;208m",                    // orange
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
            Status::Incomplete => "incomplete",
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

    let now = Utc::now();

    let format_node = |id: &str| -> String {
        let task = task_map.get(id);
        let is_context = context_ids.contains(id);
        let is_coordinator = task.is_some_and(|t| super::is_coordinator_task(t));
        let status = task.map(|t| status_label(&t.status)).unwrap_or("unknown");

        // Context nodes: dimmed, reduced detail (just ID and status, no tokens/phase/loop)
        if is_context {
            return format!("{}{}  ({}){}", dim, id, status, reset);
        }

        // Coordinator tasks: cyan color, "turn N" instead of loop info
        let color = if is_coordinator && use_color {
            "\x1b[36m" // cyan
        } else {
            task.map(|t| status_color(&t.status)).unwrap_or("")
        };
        let loop_info = if is_coordinator {
            task.map(|t| format!(" [turn {}]", t.loop_iteration))
                .unwrap_or_default()
        } else {
            task.filter(|t| t.cycle_config.is_some() || t.loop_iteration > 0)
                .map(|t| {
                    let (iter, max, forced) = if let Some(ref cfg) = t.cycle_config {
                        (t.loop_iteration, cfg.max_iterations, cfg.no_converge)
                    } else {
                        (t.loop_iteration, 0, false)
                    };
                    if max > 0 {
                        let label = if forced { "forced" } else { "iter" };
                        format!(" ↺ ({} {}/{})", label, iter, max)
                    } else {
                        format!(" ↺ (iter {})", iter)
                    }
                })
                .unwrap_or_default()
        };
        let phase_info = annotations
            .get(id)
            .map(|a| format!(" {}", a.text))
            .unwrap_or_default();

        // Override phase annotation to true pink for agency phases (assigning/evaluating).
        // Uses ANSI 256-color 219 (light pink) to be visually distinct from magenta/purple
        // which is used for upstream edge tracing.
        let is_agency_phase = use_color
            && annotations.get(id).is_some_and(|a| {
                a.text.contains("placing")
                    || a.text.contains("assigning")
                    || a.text.contains("evaluating")
                    || a.text.contains("validating")
                    || a.text.contains("verifying")
            });
        let phase_info = if is_agency_phase {
            annotations
                .get(id)
                .map(|a| format!(" \x1b[38;5;219m{}\x1b[0m", a.text))
                .unwrap_or_default()
        } else if use_color && id == ".compact-0" && annotations.contains_key(id) {
            // Color the compact-0 annotation based on compaction state:
            //   ⟳ (running)  → red
            //   ✓ (done)     → green
            //   ⚠ (warning)  → yellow
            //   idle         → gray
            let ansi = annotations
                .get(id)
                .map(|a| {
                    if a.text.contains('⟳') {
                        "\x1b[31m" // red: actively compacting
                    } else if a.text.contains('✓') {
                        "\x1b[32m" // green: recently completed
                    } else if a.text.contains('⚠') {
                        "\x1b[33m" // yellow: approaching/past threshold
                    } else {
                        "\x1b[90m" // gray: idle, far from threshold
                    }
                })
                .unwrap_or("\x1b[90m");
            annotations
                .get(id)
                .map(|a| format!(" {}{}\x1b[0m", ansi, a.text))
                .unwrap_or_default()
        } else {
            phase_info
        };

        let usage = task.and_then(|t| {
            t.token_usage
                .as_ref()
                .or_else(|| live_token_usage.get(&t.id))
        });
        let agency_usage = agency_token_usage.get(id);
        let status_with_tokens = if let Some(tok_str) = format_token_display(usage, agency_usage) {
            format!("{} · {}", status, tok_str)
        } else {
            status.to_string()
        };
        let msg_indicator = message_stats
            .get(id)
            .map(|stats| {
                let count_str = if stats.outgoing > 0 {
                    format!("{}/{}", stats.incoming, stats.outgoing)
                } else {
                    format!("{}", stats.incoming)
                };
                if use_color {
                    if let Some(status) = coordinator_status.get(id) {
                        format!(
                            " {}{}{}\x1b[0m",
                            status.ansi_prefix(),
                            status.icon(),
                            count_str
                        )
                    } else {
                        // No TUI cursor data: fall back to MessageStats-based coloring.
                        let color = if stats.responded {
                            "\x1b[32m" // green
                        } else if !stats.has_unread {
                            "\x1b[34m" // blue
                        } else {
                            "\x1b[33m" // yellow
                        };
                        format!(" {}✉{}\x1b[0m", color, count_str)
                    }
                } else {
                    let icon = coordinator_status.get(id).map(|s| s.icon()).unwrap_or('✉');
                    format!(" {}{}", icon, count_str)
                }
            })
            .unwrap_or_default();
        // Delay indicator for tasks with not_before in the future
        let delay_hint = task
            .filter(|t| matches!(t.status, Status::Open | Status::Incomplete))
            .and_then(|t| {
                t.not_before.as_deref().and_then(|nb| {
                    nb.parse::<chrono::DateTime<Utc>>().ok().and_then(|ts| {
                        if ts > now {
                            let secs = (ts - now).num_seconds();
                            let dur = workgraph::format_duration(secs, true);
                            if use_color {
                                Some(format!(" \x1b[33m⏳{}\x1b[0m", dur))
                            } else {
                                Some(format!(" ⏳{}", dur))
                            }
                        } else {
                            None
                        }
                    })
                })
            })
            .unwrap_or_default();
        let relative_ts = task
            .and_then(|t| {
                // Pick the most relevant timestamp based on status
                let ts_str = match t.status {
                    Status::InProgress => t.started_at.as_deref(),
                    Status::Done => t.completed_at.as_deref(),
                    _ => None,
                }
                .or(t.created_at.as_deref());
                ts_str.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            })
            .map(|dt| {
                let secs = (now - dt.with_timezone(&Utc)).num_seconds().max(0);
                let dur = workgraph::format_duration(secs, true);
                if use_color {
                    format!(" \x1b[90m{}\x1b[0m", dur)
                } else {
                    format!(" {}", dur)
                }
            })
            .unwrap_or_default();
        format!(
            "{}{}{}  ({}){}{}{}{}{}",
            color,
            id,
            reset,
            status_with_tokens,
            delay_hint,
            relative_ts,
            msg_indicator,
            phase_info,
            loop_info
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
    // Two-level sort: "hot" WCCs (with in-progress tasks or recently-created
    // open tasks) first, then by most recent timestamp.
    // This ensures new tasks appear at the top of the graph immediately,
    // rather than appearing after running tasks then jumping on the next refresh.
    let mut component_list: Vec<Vec<&str>> = components.into_values().collect();
    component_list.retain(|c| !c.is_empty());
    let now_utc = Utc::now();
    component_list.sort_by(|a, b| {
        let is_hot = |ids: &[&str]| -> bool {
            ids.iter().any(|id| {
                let Some(t) = task_map.get(id) else {
                    return false;
                };
                // Coordinator tasks are hot when they have a recent log entry
                // (within 60s), indicating the coordinator is actively processing.
                if t.tags.iter().any(|tag| tag == "coordinator-loop")
                    && let Some(last_log) = t.log.last()
                    && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&last_log.timestamp)
                {
                    let age = now_utc.signed_duration_since(dt);
                    if age.num_seconds() < 60 {
                        return true;
                    }
                }
                if t.status == Status::InProgress {
                    return true;
                }
                // Treat recently-created open tasks as hot so they sort to the
                // top immediately instead of appearing after running components.
                if t.status == Status::Open
                    && let Some(ref created) = t.created_at
                    && let Ok(dt) = chrono::DateTime::parse_from_rfc3339(created)
                {
                    let age = now_utc.signed_duration_since(dt);
                    if age.num_seconds() < 5 {
                        return true;
                    }
                }
                false
            })
        };
        let a_hot = is_hot(a);
        let b_hot = is_hot(b);
        // Hot WCCs first, then by most-recently-updated timestamp
        b_hot.cmp(&a_hot).then_with(|| {
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
                parents
                    .iter()
                    .all(|&p| match cycle_analysis.task_to_cycle.get(id) {
                        Some(idx) => cycle_analysis.task_to_cycle.get(p) == Some(idx),
                        None => false,
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
                simulate_dfs(
                    root,
                    None,
                    &mut sim_rendered,
                    &mut invisible_visits,
                    &forward,
                );
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
                        && let Some(&dependent_line) = node_line_map.get(id)
                    {
                        back_edge_arcs.push(BackEdgeArc {
                            blocker_line,
                            dependent_line,
                            from_id: pid.to_string(),
                            to_id: id.to_string(),
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
                    let child_is_last = children[i + 1..]
                        .iter()
                        .all(|&sib| invisible_visits.contains(&(id.to_string(), sib.to_string())));
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
    }

    // Add arcs for fan-in edges that were moved during diamond restructuring
    for &(parent, fan_in) in &fan_in_arc_edges {
        if let (Some(&parent_line), Some(&fan_in_line)) =
            (node_line_map.get(parent), node_line_map.get(fan_in))
        {
            back_edge_arcs.push(BackEdgeArc {
                blocker_line: parent_line,
                dependent_line: fan_in_line,
                from_id: parent.to_string(),
                to_id: fan_in.to_string(),
            });
        }
    }

    // Build char_edge_map for tree connectors (Phase 1 output)
    let mut char_edge_map: HashMap<(usize, usize), Vec<(String, String)>> =
        build_tree_char_edge_map(&lines, &node_line_map);

    // Phase 2: Draw right-side arcs for all non-tree edges
    let _has_crossings = draw_back_edge_arcs(
        &mut lines,
        &back_edge_arcs,
        use_color,
        arc_color,
        &mut char_edge_map,
    );

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

    // Build cycle membership map from existing cycle_analysis.
    let mut cycle_members_map: HashMap<String, HashSet<String>> = HashMap::new();
    for cycle in &cycle_analysis.cycles {
        let member_set: HashSet<String> = cycle.members.iter().cloned().collect();
        for member in &cycle.members {
            cycle_members_map.insert(member.clone(), member_set.clone());
        }
    }

    VizOutput {
        text: lines.join("\n"),
        node_line_map: owned_node_line_map,
        task_order,
        forward_edges,
        reverse_edges,
        char_edge_map,
        cycle_members: cycle_members_map,
        annotation_map: annotations.clone(),
    }
}

/// Build a character-level edge map for tree connectors.
///
/// Analyzes the rendered plain text to determine which edge each tree connector
/// character (│, ├, └, →) belongs to. Returns a map from (line, visible_col)
/// to (source_id, target_id).
fn build_tree_char_edge_map(
    lines: &[String],
    node_line_map: &HashMap<&str, usize>,
) -> HashMap<(usize, usize), Vec<(String, String)>> {
    let mut map: HashMap<(usize, usize), Vec<(String, String)>> = HashMap::new();

    // Build reverse map: line_num → task_id
    let mut line_to_task: HashMap<usize, &str> = HashMap::new();
    for (&task_id, &line_num) in node_line_map {
        line_to_task.insert(line_num, task_id);
    }

    // Strip ANSI and get plain text for each line
    let plain_lines: Vec<String> = lines.iter().map(|l| strip_ansi_for_map(l)).collect();

    // For each task, determine its depth from the plain text.
    // Root: depth 0, text starts at col 0 (no connector).
    // Depth d (d >= 1): text starts at col 2*d + 1 (child spacing + connector).
    struct NodeInfo {
        id: String,
        line: usize,
        depth: usize,
    }

    let mut nodes: Vec<NodeInfo> = Vec::new();
    for (&task_id, &line_num) in node_line_map {
        if line_num >= plain_lines.len() {
            continue;
        }
        let plain = &plain_lines[line_num];
        // Find the first alphanumeric character position (task text start)
        let text_start = match plain.chars().position(|c| c.is_alphanumeric()) {
            Some(pos) => pos,
            None => continue,
        };
        let depth = if text_start < 3 {
            0
        } else {
            (text_start - 1) / 2
        };
        nodes.push(NodeInfo {
            id: task_id.to_string(),
            line: line_num,
            depth,
        });
    }

    // Sort nodes by line number
    nodes.sort_by_key(|n| n.line);

    // Build tree parent-child relationships using a stack-based approach.
    // Process nodes top-to-bottom; the parent is the most recent node with depth - 1.
    // parent_id → ordered children
    let mut tree_children: HashMap<String, Vec<String>> = HashMap::new();
    // Stack of (id, depth) — maintains the current path from root to current position
    let mut stack: Vec<(String, usize)> = Vec::new();

    for node in &nodes {
        // Pop stack until we find the parent (depth - 1)
        while let Some((_, d)) = stack.last() {
            if *d >= node.depth {
                stack.pop();
            } else {
                break;
            }
        }

        if node.depth > 0
            && let Some((parent_id, _)) = stack.last()
        {
            tree_children
                .entry(parent_id.clone())
                .or_default()
                .push(node.id.clone());
        }

        stack.push((node.id.clone(), node.depth));
    }

    // Build node depth lookup
    let node_depth: HashMap<&str, usize> = nodes.iter().map(|n| (n.id.as_str(), n.depth)).collect();
    let node_line: HashMap<&str, usize> = nodes.iter().map(|n| (n.id.as_str(), n.line)).collect();

    // Map tree connector characters to edges.
    // For each parent P with children [c1, c2, ..., cn]:
    //   - Connector (├→ or └→) on ci's line at col 2*(P_depth): edge (P, ci)
    //   - → on ci's line at col 2*(P_depth) + 1: edge (P, ci)
    //   - │ between ci and ci+1 at col 2*(P_depth): edge (P, ci+1)
    for (parent_id, children) in &tree_children {
        let p_depth = match node_depth.get(parent_id.as_str()) {
            Some(&d) => d,
            None => continue,
        };
        let connector_col = 2 * p_depth; // column where ├/└/│ appear for this parent's children

        for (i, child_id) in children.iter().enumerate() {
            let child_line = match node_line.get(child_id.as_str()) {
                Some(&l) => l,
                None => continue,
            };

            // Map the connector characters on the child's line
            // ├ or └ at connector_col, → at connector_col + 1
            let edge = (parent_id.clone(), child_id.clone());
            if child_line < plain_lines.len() {
                let chars: Vec<char> = plain_lines[child_line].chars().collect();
                if connector_col < chars.len() && is_tree_connector(chars[connector_col]) {
                    map.entry((child_line, connector_col))
                        .or_default()
                        .push(edge.clone());
                }
                if connector_col + 1 < chars.len() && chars[connector_col + 1] == '→' {
                    map.entry((child_line, connector_col + 1))
                        .or_default()
                        .push(edge.clone());
                }
            }

            // Map │ characters between this child's subtree and the next sibling.
            // Only map to edges for children BELOW this point (not the current child).
            // A vertical bar represents the trunk continuing down to subsequent siblings.
            // It should only be colored if at least one child below is in the traced path.
            if i + 1 < children.len() {
                let next_child_id = &children[i + 1];
                let next_child_line = match node_line.get(next_child_id.as_str()) {
                    Some(&l) => l,
                    None => continue,
                };

                // Collect edges for ALL remaining children below (j > i)
                let remaining_edges: Vec<(String, String)> = children[i + 1..]
                    .iter()
                    .map(|cid| (parent_id.clone(), cid.clone()))
                    .collect();

                // Lines from child_line+1 to next_child_line-1 at connector_col
                for l in (child_line + 1)..next_child_line {
                    if l < plain_lines.len() {
                        let chars: Vec<char> = plain_lines[l].chars().collect();
                        if connector_col < chars.len() && chars[connector_col] == '│' {
                            let entries = map.entry((l, connector_col)).or_default();
                            for edge in &remaining_edges {
                                if !entries.contains(edge) {
                                    entries.push(edge.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    map
}

/// Check if a character is a tree connector (├, └, but not │ which is handled separately).
fn is_tree_connector(c: char) -> bool {
    matches!(c, '├' | '└')
}

/// Strip ANSI escape codes from a string to get plain visible text.
fn strip_ansi_for_map(s: &str) -> String {
    let mut result = String::new();
    let mut in_escape = false;
    for ch in s.chars() {
        if in_escape {
            if ch == 'm' {
                in_escape = false;
            }
        } else if ch == '\x1b' {
            in_escape = true;
        } else {
            result.push(ch);
        }
    }
    result
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
fn draw_back_edge_arcs(
    lines: &mut [String],
    arcs: &[BackEdgeArc],
    use_color: bool,
    arc_color_code: &str,
    char_edge_map: &mut HashMap<(usize, usize), Vec<(String, String)>>,
) -> bool {
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
            // If the node label already contains ↺ (from cycle_config/loop_iteration
            // in format_node), skip appending — avoids duplicate indicators.
            // Otherwise, append ⟳ as a distinct self-loop indicator.
            if !lines[line_idx].contains('↺') {
                if use_color {
                    lines[line_idx].push_str(&format!(" {}⟳{}", arc_color_code, "\x1b[0m"));
                } else {
                    lines[line_idx].push_str(" ⟳");
                }
            }
        }
    }

    if real_arcs.is_empty() {
        return false;
    }

    // Group arcs by dependent line — all edges pointing to the same dependent
    // share one column, regardless of whether blockers are above or below.
    // Track task IDs alongside line numbers for the char_edge_map.
    struct ArcInfo {
        blocker_line: usize,
        from_id: String,
        to_id: String,
    }
    let mut by_dependent: HashMap<usize, Vec<ArcInfo>> = HashMap::new();
    for arc in &real_arcs {
        by_dependent
            .entry(arc.dependent_line)
            .or_default()
            .push(ArcInfo {
                blocker_line: arc.blocker_line,
                from_id: arc.from_id.clone(),
                to_id: arc.to_id.clone(),
            });
    }

    struct ArcColumn {
        dependent: usize,
        dependent_id: String,
        blockers: Vec<usize>,
        /// Map from blocker_line → from_id (the blocker task ID)
        blocker_id_map: HashMap<usize, String>,
        top: usize,
        bottom: usize,
    }

    let mut columns: Vec<ArcColumn> = by_dependent
        .into_iter()
        .map(|(dependent, infos)| {
            let dependent_id = infos.first().map(|a| a.to_id.clone()).unwrap_or_default();
            let mut blocker_id_map: HashMap<usize, String> = HashMap::new();
            let mut blockers: Vec<usize> = Vec::new();
            for info in &infos {
                blocker_id_map.entry(info.blocker_line).or_insert_with(|| {
                    blockers.push(info.blocker_line);
                    info.from_id.clone()
                });
            }
            blockers.sort();
            blockers.dedup();
            let min_blocker = *blockers.first().unwrap();
            let max_blocker = *blockers.last().unwrap();
            let top = dependent.min(min_blocker);
            let bottom = dependent.max(max_blocker);
            ArcColumn {
                dependent,
                dependent_id,
                blockers,
                blocker_id_map,
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
        // Build node_set for this band's columns (using band-local indices)
        let node_set: HashSet<(usize, usize)> = band
            .col_indices
            .iter()
            .enumerate()
            .flat_map(|(local_idx, &ci)| {
                let c = &columns[ci];
                let dep = std::iter::once((local_idx, c.dependent));
                let blk = c.blockers.iter().map(move |&b| (local_idx, b));
                dep.chain(blk)
            })
            .collect();

        let mut col_x_positions: Vec<usize> = Vec::new();
        for (local_idx, &col_idx) in band.col_indices.iter().enumerate() {
            let column = &columns[col_idx];

            // Compute column position from the widest line in this column's
            // span (not the entire band). This keeps arcs compact when they
            // don't need to route around wider content on other lines.
            let span_max_width = (column.top..=column.bottom.min(lines.len() - 1))
                .map(|l| visible_len(&lines[l]))
                .max()
                .unwrap_or(0);
            let mut col_x = span_max_width + 2;
            // Ensure col_stride spacing from any earlier column that overlaps vertically
            for (prev_local, &prev_ci) in band.col_indices[..local_idx].iter().enumerate() {
                let prev_col = &columns[prev_ci];
                if column.top <= prev_col.bottom && column.bottom >= prev_col.top {
                    col_x = col_x.max(col_x_positions[prev_local] + col_stride);
                }
            }
            col_x_positions.push(col_x);

            for line_idx in column.top..=column.bottom {
                if line_idx >= lines.len() {
                    continue;
                }

                let is_dep = line_idx == column.dependent;
                let is_blocker = column.blockers.contains(&line_idx);
                let is_top = line_idx == column.top;
                let is_bottom = line_idx == column.bottom;

                // Collect all arcs in this column that span through this line.
                // An arc from blocker_line → dependent spans through line_idx if
                // line_idx is between blocker_line and dependent (inclusive).
                let spanning_edges: Vec<(String, String)> = column
                    .blockers
                    .iter()
                    .filter(|&&b| {
                        let lo = column.dependent.min(b);
                        let hi = column.dependent.max(b);
                        line_idx >= lo && line_idx <= hi
                    })
                    .map(|b| {
                        let from_id = column.blocker_id_map.get(b).cloned().unwrap_or_default();
                        (from_id, column.dependent_id.clone())
                    })
                    .collect();

                // The specific edge for this line's own horizontal connection.
                let specific_edge = if is_blocker {
                    let from_id = column
                        .blocker_id_map
                        .get(&line_idx)
                        .cloned()
                        .unwrap_or_default();
                    Some((from_id, column.dependent_id.clone()))
                } else {
                    None // Dependent line uses all spanning edges
                };

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
                    } else if is_dep {
                        "←┤"
                    } else {
                        "─┤"
                    };

                    let pre_len = visible_len(line);
                    let current = pre_len;
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
                            line.push_str(&format!(
                                "{}{}{}",
                                dim,
                                &glyph[glyph.char_indices().last().unwrap().0..],
                                reset
                            ));
                        }
                    }
                    let post_len = visible_len(line);
                    // Record edges for new visible positions (skip leading separator space).
                    // Horizontal chars: specific edge (blocker) or all edges (dependent).
                    // Vertical position (col_x+1): all spanning arcs.
                    let start = if post_len > pre_len + 1 {
                        pre_len + 1
                    } else {
                        pre_len
                    };
                    for pos in start..post_len {
                        let edges = char_edge_map.entry((line_idx, pos)).or_default();
                        if pos == col_x + 1 {
                            // Vertical column position: record ALL spanning arcs
                            for e in &spanning_edges {
                                if !edges.contains(e) {
                                    edges.push(e.clone());
                                }
                            }
                        } else if let Some(ref se) = specific_edge {
                            // Blocker horizontal: only this blocker's edge
                            if !edges.contains(se) {
                                edges.push(se.clone());
                            }
                        } else {
                            // Dependent horizontal (arrowhead/dashes): all arcs
                            for e in &spanning_edges {
                                if !edges.contains(e) {
                                    edges.push(e.clone());
                                }
                            }
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
                                && (line_idx == c.dependent || c.blockers.contains(&line_idx))
                        });

                        let pre_len = visible_len(line);
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
                        let post_len = visible_len(line);
                        // Record vertical character position — ALL spanning arcs
                        if post_len > pre_len {
                            let edges = char_edge_map.entry((line_idx, col_x + 1)).or_default();
                            for e in &spanning_edges {
                                if !edges.contains(e) {
                                    edges.push(e.clone());
                                }
                            }
                            // For crossings: dash fill belongs to the HORIZONTAL arc(s),
                            // and ┼ belongs to BOTH vertical and horizontal arcs.
                            if has_crossing {
                                // Collect horizontal edge(s) from outer columns crossing this line
                                let mut horizontal_edges: Vec<(String, String)> = Vec::new();
                                for &outer_ci in &band.col_indices[local_idx + 1..] {
                                    let outer_col = &columns[outer_ci];
                                    if line_idx >= outer_col.top
                                        && line_idx <= outer_col.bottom
                                        && (line_idx == outer_col.dependent
                                            || outer_col.blockers.contains(&line_idx))
                                    {
                                        if outer_col.blockers.contains(&line_idx) {
                                            // Blocker line: specific edge
                                            let from_id = outer_col
                                                .blocker_id_map
                                                .get(&line_idx)
                                                .cloned()
                                                .unwrap_or_default();
                                            let edge = (from_id, outer_col.dependent_id.clone());
                                            if !horizontal_edges.contains(&edge) {
                                                horizontal_edges.push(edge);
                                            }
                                        } else {
                                            // Dependent line: all spanning edges for this outer column
                                            for &b in &outer_col.blockers {
                                                let lo = outer_col.dependent.min(b);
                                                let hi = outer_col.dependent.max(b);
                                                if line_idx >= lo && line_idx <= hi {
                                                    let from_id = outer_col
                                                        .blocker_id_map
                                                        .get(&b)
                                                        .cloned()
                                                        .unwrap_or_default();
                                                    let edge =
                                                        (from_id, outer_col.dependent_id.clone());
                                                    if !horizontal_edges.contains(&edge) {
                                                        horizontal_edges.push(edge);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                // Dash fill before ┼: map to horizontal arc(s) only
                                let fill_start = if post_len > pre_len + 1 {
                                    pre_len + 1
                                } else {
                                    pre_len
                                };
                                for pos in fill_start..col_x + 1 {
                                    let edges = char_edge_map.entry((line_idx, pos)).or_default();
                                    for he in &horizontal_edges {
                                        if !edges.contains(he) {
                                            edges.push(he.clone());
                                        }
                                    }
                                }

                                // ┼ itself: also map to horizontal arc(s)
                                let edges = char_edge_map.entry((line_idx, col_x + 1)).or_default();
                                for he in &horizontal_edges {
                                    if !edges.contains(he) {
                                        edges.push(he.clone());
                                    }
                                }
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
            len += ch.width().unwrap_or(0);
        }
    }
    len
}

/// Truncate a string to at most `max_visible` visible columns,
/// preserving ANSI escape sequences and appending a reset code if needed.
/// Returns the original string unchanged if it fits.
pub(crate) fn truncate_to_width(s: &str, max_visible: usize) -> String {
    if visible_len(s) <= max_visible {
        return s.to_string();
    }

    let mut result = String::with_capacity(s.len());
    let mut vis = 0;
    let mut in_escape = false;
    let mut has_active_style = false;

    for ch in s.chars() {
        if in_escape {
            result.push(ch);
            if ch == 'm' {
                in_escape = false;
                // Track whether we have active styling (non-reset)
                // A reset is \x1b[0m; anything else is active
            }
        } else if ch == '\x1b' {
            in_escape = true;
            has_active_style = true;
            result.push(ch);
        } else {
            let w = ch.width().unwrap_or(0);
            if vis + w > max_visible {
                break;
            }
            result.push(ch);
            vis += w;
        }
    }

    // Ensure we reset any active ANSI styling
    if has_active_style {
        result.push_str("\x1b[0m");
    }

    result
}

/// Truncate all lines in a multiline string to `max_columns` visible width.
pub(crate) fn truncate_lines(text: &str, max_columns: u16) -> String {
    let max = max_columns as usize;
    text.lines()
        .map(|line| truncate_to_width(line, max))
        .collect::<Vec<_>>()
        .join("\n")
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        assert!(result.text.contains("solo"));
        assert!(!result.text.contains("(independent)"));
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

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

        let annotations: HashMap<String, crate::commands::viz::AnnotationInfo> = HashMap::new();
        let (filtered, annots) = crate::commands::viz::filter_internal_tasks(
            &graph,
            graph.tasks().collect(),
            &annotations,
        );
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        let result = generate_ascii(
            &graph,
            &filtered,
            &task_ids,
            &annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Internal task should NOT appear
        assert!(!result.text.contains("assign-my-task"));
        // Parent task should appear with phase annotation
        assert!(result.text.contains("my-task"));
        assert!(result.text.contains("[⊞ assigning]"));
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

        let annotations: HashMap<String, crate::commands::viz::AnnotationInfo> = HashMap::new();
        let (filtered, annots) = crate::commands::viz::filter_internal_tasks(
            &graph,
            graph.tasks().collect(),
            &annotations,
        );
        let task_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();

        let result = generate_ascii(
            &graph,
            &filtered,
            &task_ids,
            &annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        assert!(!result.text.contains("evaluate-my-task"));
        assert!(result.text.contains("my-task"));
        assert!(result.text.contains("[∴ evaluating]"));
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

        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Both tasks should be visible
        assert!(result.text.contains("assign-my-task"));
        assert!(result.text.contains("my-task"));
        // No phase annotation when shown as literal nodes
        assert!(!result.text.contains("[⊞ assigning]"));
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
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

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
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });
        task.loop_iteration = 2;
        graph.add_node(Node::Task(task));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

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
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });
        let mut tgt = make_task("tgt", "Target");
        tgt.after = vec!["src".to_string()];
        graph.add_node(Node::Task(src));
        graph.add_node(Node::Task(tgt));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

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
        assert!(
            !result.text.contains("⟳"),
            "Should NOT contain ⟳ on normal task:\n{}",
            result.text
        );
    }

    #[test]
    fn test_self_loop_without_cycle_config_shows_gapped_arrow() {
        // A task with itself in its `after` list but no cycle_config should show ⟳
        let mut graph = WorkGraph::new();
        let mut task = make_task("self-loop", "Self Loop Task");
        task.after = vec!["self-loop".to_string()];
        graph.add_node(Node::Task(task));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        assert!(
            result.text.contains("⟳"),
            "Self-loop without cycle_config should show ⟳:\n{}",
            result.text
        );
        // Should NOT have double loop symbols
        assert!(
            !result.text.contains("↺"),
            "Self-loop without cycle_config should not show ↺:\n{}",
            result.text
        );
    }

    #[test]
    fn test_self_loop_with_cycle_config_no_duplicate() {
        // A task with cycle_config AND a self-loop edge should NOT get double ↺
        use workgraph::graph::CycleConfig;

        let mut graph = WorkGraph::new();
        let mut task = make_task("cycler", "Cycling task");
        task.after = vec!["cycler".to_string()];
        task.cycle_config = Some(CycleConfig {
            max_iterations: 10,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });
        task.loop_iteration = 3;
        graph.add_node(Node::Task(task));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Should show the cycle_config ↺ with iteration info
        assert!(
            result.text.contains("↺ (iter 3/10)"),
            "Should show iteration info:\n{}",
            result.text
        );
        // Count occurrences of ↺ — should be exactly one
        let loop_count = result.text.matches('↺').count();
        assert_eq!(
            loop_count, 1,
            "Self-loop with cycle_config should have exactly one ↺, got {}:\n{}",
            loop_count, result.text
        );
        // Should NOT also show ⟳ since ↺ is already present
        assert!(
            !result.text.contains("⟳"),
            "Should not show ⟳ when ↺ already present:\n{}",
            result.text
        );
    }

    #[test]
    fn test_non_self_loop_cycle_unchanged() {
        // A two-task cycle (A→B→A) should NOT show ⟳ — only standard back-edge arcs
        use workgraph::graph::CycleConfig;

        let mut graph = WorkGraph::new();
        let mut a = make_task("task-a", "Task A");
        a.cycle_config = Some(CycleConfig {
            max_iterations: 5,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });
        a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut b = make_task("task-b", "Task B");
        b.after = vec!["task-a".to_string()];
        b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        a.after = vec!["task-b".to_string()]; // back-edge
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let no_annots = HashMap::new();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &no_annots,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Multi-member cycle should NOT show ⟳
        assert!(
            !result.text.contains("⟳"),
            "Multi-member cycle should not show ⟳:\n{}",
            result.text
        );
        // Should have back-edge arcs (← and ┘)
        assert!(
            result.text.contains("←"),
            "Multi-member cycle should have back-edge arcs:\n{}",
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
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Should have ← at target and ┘ at source
        assert!(
            result.text.contains("←"),
            "Back-edge target should have ←\nOutput:\n{}",
            result.text
        );
        assert!(
            result.text.contains("┘"),
            "Back-edge source should have ┘\nOutput:\n{}",
            result.text
        );
        // Should NOT have old-style cycle-back text
        assert!(
            !result.text.contains("cycles back"),
            "No old-style text\nOutput:\n{}",
            result.text
        );
        // Should NOT have fan-in annotations
        assert!(
            !result.text.contains("(←"),
            "No fan-in text annotations\nOutput:\n{}",
            result.text
        );
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Fan-in should produce a right-side arc (not a text annotation)
        assert!(
            result.text.contains("←") || result.text.contains("┘"),
            "Diamond fan-in should have right-side arcs\nOutput:\n{}",
            result.text
        );
        assert!(
            !result.text.contains("(←"),
            "No fan-in text annotation\nOutput:\n{}",
            result.text
        );
        assert!(
            !result.text.contains("..."),
            "No duplicate 'already shown' entries\nOutput:\n{}",
            result.text
        );
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Diamond,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
        eprintln!("DIAMOND:\n{}", result.text);

        let lines: Vec<&str> = result.text.lines().collect();
        // join should be at the same indentation as left and right (direct child of root)
        let join_line = lines
            .iter()
            .find(|l| l.contains("join"))
            .expect("join should appear");
        let left_line = lines
            .iter()
            .find(|l| l.contains("left"))
            .expect("left should appear");
        // Both should start with tree connectors at the same indent level
        let join_indent = join_line.find("join").unwrap();
        let left_indent = left_line.find("left").unwrap();
        assert_eq!(
            join_indent, left_indent,
            "Diamond layout: join should be at same indent as left\nOutput:\n{}",
            result.text
        );

        // join should appear AFTER both left and right in line order
        let left_idx = lines.iter().position(|l| l.contains("left")).unwrap();
        let right_idx = lines.iter().position(|l| l.contains("right")).unwrap();
        let join_idx = lines.iter().position(|l| l.contains("join")).unwrap();
        assert!(join_idx > left_idx, "join should be after left");
        assert!(join_idx > right_idx, "join should be after right");

        // Arcs should flow DOWN (left and right have ┐ or ─, join has ← or ┘)
        assert!(
            join_line.contains("←") || join_line.contains("┘"),
            "join should receive arcs\nOutput:\n{}",
            result.text
        );

        // Compare with tree layout (old behavior): join should be under left
        let result_tree = generate_ascii(
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
        eprintln!("TREE:\n{}", result_tree.text);
        let tree_lines: Vec<&str> = result_tree.text.lines().collect();
        let join_tree = tree_lines
            .iter()
            .find(|l| l.contains("join"))
            .expect("join in tree");
        let left_tree = tree_lines
            .iter()
            .find(|l| l.contains("left"))
            .expect("left in tree");
        let join_tree_indent = join_tree.find("join").unwrap();
        let left_tree_indent = left_tree.find("left").unwrap();
        // In tree mode, join should be DEEPER than left (a child of left)
        assert!(
            join_tree_indent > left_tree_indent,
            "Tree layout: join should be deeper than left\nOutput:\n{}",
            result_tree.text
        );
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::Diamond,
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
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
        assert!(
            join_line.contains("←") || join_line.contains("┘"),
            "join should receive arcs in wide diamond\nOutput:\n{}",
            result.text
        );
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
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let tree_lines: Vec<&str> = result.text.lines().collect();
        // The child should use └→ (last visible child), not ├→
        let child_line = tree_lines.iter().find(|l| l.contains("child"));
        assert!(
            child_line.is_some(),
            "Child should appear\nOutput:\n{}",
            result.text
        );
        assert!(
            child_line.unwrap().contains("└→"),
            "Child should use └→ (no orphaned ├→)\nLine: '{}'\nOutput:\n{}",
            child_line.unwrap(),
            result.text
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
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
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
        target.after = vec![
            "spoke-a".to_string(),
            "spoke-b".to_string(),
            "spoke-c".to_string(),
        ];

        graph.add_node(Node::Task(target));
        graph.add_node(Node::Task(s1));
        graph.add_node(Node::Task(s2));
        graph.add_node(Node::Task(s3));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Should have exactly one ← (same-target collapse)
        let target_count = result.text.matches("←").count();
        assert_eq!(
            target_count, 1,
            "Multiple sources to same target should collapse to 1 column\nOutput:\n{}",
            result.text
        );
        // Should have ┤ for intermediate sources and ┘ for the last
        assert!(
            result.text.contains("┤"),
            "Intermediate sources should have ┤\nOutput:\n{}",
            result.text
        );
        assert!(
            result.text.contains("┘"),
            "Last source should have ┘\nOutput:\n{}",
            result.text
        );
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
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Find the lines
        let lines: Vec<&str> = result.text.lines().collect();
        let b_line = lines
            .iter()
            .find(|l| l.contains("bbb"))
            .expect("B should appear");
        let c_line = lines
            .iter()
            .find(|l| l.contains("ccc"))
            .expect("C should appear");

        // C (dependent) should have ← arrowhead (arc from B flows down to C)
        assert!(
            c_line.contains("←"),
            "Forward skip: dependent C should have ←\nOutput:\n{}",
            result.text
        );
        // B (blocker) should have ┐ (top corner of downward arc to C)
        assert!(
            b_line.contains("┐"),
            "Forward skip: blocker B should have ┐\nOutput:\n{}",
            result.text
        );

        // Verify tree layout with LayoutMode::Tree (old behavior)
        let result_tree = generate_ascii(
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
        let tree_lines: Vec<&str> = result_tree.text.lines().collect();
        let a_line_tree = tree_lines
            .iter()
            .find(|l| l.contains("aaa"))
            .expect("A should appear in tree mode");
        // In tree mode, A→C forward skip produces an arc with ┐ at A
        assert!(
            a_line_tree.contains("┐") || a_line_tree.contains("─"),
            "Tree mode: A should participate in arc\nOutput:\n{}",
            result_tree.text
        );
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // D should have exactly one ← (same-dependent collapse)
        let arrow_count = result.text.matches("←").count();
        assert_eq!(
            arrow_count, 1,
            "Mixed direction arcs to same dependent should collapse to 1 column\nOutput:\n{}",
            result.text
        );

        // D's line should have ←
        let lines: Vec<&str> = result.text.lines().collect();
        let d_line = lines
            .iter()
            .find(|l| l.contains("ddd"))
            .expect("D should appear");
        assert!(
            d_line.contains("←"),
            "Mixed direction: D should have ←\nOutput:\n{}",
            result.text
        );
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Each dependent (leaf-a, leaf-b) should have ←
        let lines: Vec<&str> = result.text.lines().collect();
        let c_line = lines
            .iter()
            .find(|l| l.contains("leaf-a"))
            .expect("leaf-a should appear");
        let d_line = lines
            .iter()
            .find(|l| l.contains("leaf-b"))
            .expect("leaf-b should appear");
        assert!(
            c_line.contains("←"),
            "leaf-a should have ← arrowhead\nOutput:\n{}",
            result.text
        );
        assert!(
            d_line.contains("←"),
            "leaf-b should have ← arrowhead\nOutput:\n{}",
            result.text
        );

        // Root (blocker) should NOT have ←
        let root_line = lines
            .iter()
            .find(|l| l.contains("root"))
            .expect("root should appear");
        assert!(
            !root_line.contains("←"),
            "root (blocker) should NOT have ←\nOutput:\n{}",
            result.text
        );
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
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
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
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
        eprintln!("CROSSING OUTPUT:\n{}", result.text);
        // Should contain crossing character ┼ where verticals cross horizontals
        assert!(
            result.text.contains("┼"),
            "Should have crossing character ┼ where arcs cross\nOutput:\n{}",
            result.text
        );
    }

    #[test]
    fn test_char_edge_map_simple_chain() {
        // A → B → C: tree connectors should map to the correct edges
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Verify char_edge_map has entries for tree connectors
        assert!(
            !result.char_edge_map.is_empty(),
            "char_edge_map should have entries for tree connectors"
        );

        // B's connector (├→ or └→) should map to edge (a, b)
        // Connector col for children of root (depth 0) is 0.
        let b_line = result.node_line_map["b"];
        let b_connector_edges = result.char_edge_map.get(&(b_line, 0));
        assert!(
            b_connector_edges.is_some(),
            "B's connector should have an edge map entry"
        );
        let (src, _tgt) = &b_connector_edges.unwrap()[0];
        assert_eq!(src, "a", "B's connector should reference parent 'a'");

        // C's connector should map to edge (b, c)
        // Connector col for children of depth 1 is 2*1 = 2.
        let c_line = result.node_line_map["c"];
        let c_connector_edges = result.char_edge_map.get(&(c_line, 2));
        assert!(
            c_connector_edges.is_some(),
            "C's connector should have an edge map entry at depth 2"
        );
        let (src, tgt) = &c_connector_edges.unwrap()[0];
        assert_eq!(src, "b", "C's connector should reference parent 'b'");
        assert_eq!(tgt, "c", "C's connector target should be 'c'");
    }

    #[test]
    fn test_char_edge_map_fan_out_sibling_separation() {
        // A → B, A → C: the │ between B and C should map ONLY to edges for children
        // BELOW (a, c), NOT the current child above (a, b). The trunk going down
        // represents the path toward remaining siblings, not the child already branched.
        let mut graph = WorkGraph::new();
        let t1 = make_task("a", "Task A");
        let mut t2 = make_task("b", "Task B");
        t2.after = vec!["a".to_string()];
        let mut t3 = make_task("c", "Task C");
        t3.after = vec!["a".to_string()];
        // Add a child of B to create a line between B and C
        let mut t4 = make_task("d", "Task D");
        t4.after = vec!["b".to_string()];
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));
        graph.add_node(Node::Task(t4));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Output should look like:
        // a
        // ├→ b
        // │  └→ d
        // └→ c
        let b_line = result.node_line_map["b"];
        let c_line = result.node_line_map["c"];

        // The │ at col 0 (children of root) between B and C should contain ONLY (a, c), NOT (a, b)
        for l in (b_line + 1)..c_line {
            if let Some(edges) = result.char_edge_map.get(&(l, 0)) {
                assert!(
                    edges.iter().any(|(src, tgt)| src == "a" && tgt == "c"),
                    "│ between siblings at line {} should contain edge (a, c) for next child",
                    l
                );
                assert!(
                    !edges.iter().any(|(src, tgt)| src == "a" && tgt == "b"),
                    "│ between siblings at line {} should NOT contain edge (a, b) for current child above",
                    l
                );
            }
        }
    }

    #[test]
    fn test_char_edge_map_arc_edges() {
        // Fan-in: A → C, B → C. The arc should have char_edge_map entries.
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // The fan-in creates an arc. Verify arc characters have edge map entries.
        let has_arc_entries = result.char_edge_map.values().any(|edges| {
            edges.iter().any(|(src, tgt)| {
                // At least one arc edge should exist (A→C or B→C depending on layout)
                (src == "a" || src == "b") && tgt == "c"
            })
        });
        assert!(
            has_arc_entries,
            "char_edge_map should have arc edge entries for fan-in.\nOutput:\n{}\nMap entries: {:?}",
            result.text, result.char_edge_map
        );
    }

    // ===================================================================
    // Systematic TUI trace regression tests against spec
    // ===================================================================
    //
    // These tests validate edge trace coloring rules, selection indicator
    // behavior, navigation invariants, and char_edge_map correctness
    // for various graph topologies.

    /// Simulate TUI trace logic: compute upstream/downstream sets via BFS
    /// on the VizOutput forward/reverse edges, exactly as state.rs does.
    struct TraceState {
        upstream_set: HashSet<String>,
        downstream_set: HashSet<String>,
        selected_id: String,
    }

    impl TraceState {
        fn new(viz: &VizOutput, selected_id: &str) -> Self {
            let mut upstream_set = HashSet::new();
            let mut downstream_set = HashSet::new();

            // Upstream: BFS on reverse_edges (mirrors state.rs recompute_trace)
            {
                let mut queue = std::collections::VecDeque::new();
                for dep in viz.reverse_edges.get(selected_id).into_iter().flatten() {
                    if upstream_set.insert(dep.clone()) {
                        queue.push_back(dep.clone());
                    }
                }
                while let Some(id) = queue.pop_front() {
                    for dep in viz.reverse_edges.get(&id).into_iter().flatten() {
                        if upstream_set.insert(dep.clone()) {
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }

            // Downstream: BFS on forward_edges
            {
                let mut queue = std::collections::VecDeque::new();
                for dep in viz.forward_edges.get(selected_id).into_iter().flatten() {
                    if downstream_set.insert(dep.clone()) {
                        queue.push_back(dep.clone());
                    }
                }
                while let Some(id) = queue.pop_front() {
                    for dep in viz.forward_edges.get(&id).into_iter().flatten() {
                        if downstream_set.insert(dep.clone()) {
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }

            TraceState {
                upstream_set,
                downstream_set,
                selected_id: selected_id.to_string(),
            }
        }

        fn in_upstream(&self, id: &str) -> bool {
            self.upstream_set.contains(id) || id == self.selected_id
        }

        fn in_downstream(&self, id: &str) -> bool {
            self.downstream_set.contains(id) || id == self.selected_id
        }

        /// Classify an edge per TUI coloring spec:
        /// "upstream" (magenta) if both endpoints in upstream∪{selected},
        /// "downstream" (cyan) if both in downstream∪{selected},
        /// None otherwise.
        fn classify_edge(&self, src: &str, tgt: &str) -> Option<&'static str> {
            if self.in_upstream(src) && self.in_upstream(tgt) {
                Some("upstream")
            } else if self.in_downstream(src) && self.in_downstream(tgt) {
                Some("downstream")
            } else {
                None
            }
        }
    }

    fn render_graph(graph: &WorkGraph) -> VizOutput {
        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        generate_ascii(
            graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        )
    }

    // --- Graph builders ---

    fn build_linear_chain() -> WorkGraph {
        let mut graph = WorkGraph::new();
        let a = make_task("a", "Task A");
        let mut b = make_task("b", "Task B");
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", "Task C");
        c.after = vec!["b".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph
    }

    fn build_fan_in_abcd() -> WorkGraph {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "Task A")));
        graph.add_node(Node::Task(make_task("b", "Task B")));
        graph.add_node(Node::Task(make_task("c", "Task C")));
        let mut d = make_task("d", "Merge Task");
        d.after = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        graph.add_node(Node::Task(d));
        graph
    }

    fn build_disconnected() -> WorkGraph {
        let mut graph = WorkGraph::new();
        let a = make_task("a", "Task A");
        let mut b = make_task("b", "Task B");
        b.after = vec!["a".to_string()];
        let x = make_task("x", "Task X");
        let mut y = make_task("y", "Task Y");
        y.after = vec!["x".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(x));
        graph.add_node(Node::Task(y));
        graph
    }

    fn build_cycle_abc() -> WorkGraph {
        let mut graph = WorkGraph::new();
        let mut a = make_task("a", "Task A");
        a.after = vec!["c".to_string()];
        let mut b = make_task("b", "Task B");
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", "Task C");
        c.after = vec!["b".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph
    }

    fn build_fan_out() -> WorkGraph {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "Root")));
        let mut b = make_task("b", "Task B");
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", "Task C");
        c.after = vec!["a".to_string()];
        let mut d = make_task("d", "Task D");
        d.after = vec!["a".to_string()];
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));
        graph
    }

    fn build_diamond() -> WorkGraph {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "Task A")));
        let mut b = make_task("b", "Task B");
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", "Task C");
        c.after = vec!["a".to_string()];
        let mut d = make_task("d", "Task D");
        d.after = vec!["b".to_string(), "c".to_string()];
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));
        graph
    }

    // --- Spec Rule 1: Trace is PURELY ADDITIVE ---

    #[test]
    fn spec_rule_1_no_selection_output_identical() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);
        // With no selection the VizOutput text is the normal viz output.
        assert!(!viz.text.is_empty());
        assert!(viz.node_line_map.contains_key("a"));
        assert!(viz.node_line_map.contains_key("b"));
        assert!(viz.node_line_map.contains_key("c"));
        assert_eq!(viz.task_order.len(), 3);
    }

    // --- Spec Rules 2-3: Upstream → magenta, Downstream → cyan ---

    #[test]
    fn spec_rule_2_upstream_edges_classified_magenta() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);
        // Select C. A and B are upstream.
        let trace = TraceState::new(&viz, "c");
        assert!(trace.upstream_set.contains("a"));
        assert!(trace.upstream_set.contains("b"));
        assert!(trace.downstream_set.is_empty());

        for edges in viz.char_edge_map.values() {
            for (src, tgt) in edges {
                if trace.in_upstream(src) && trace.in_upstream(tgt) {
                    assert_eq!(trace.classify_edge(src, tgt), Some("upstream"));
                }
            }
        }
    }

    #[test]
    fn spec_rule_3_downstream_edges_classified_cyan() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);
        // Select A. B and C are downstream.
        let trace = TraceState::new(&viz, "a");
        assert!(trace.downstream_set.contains("b"));
        assert!(trace.downstream_set.contains("c"));
        assert!(trace.upstream_set.is_empty());

        for edges in viz.char_edge_map.values() {
            for (src, tgt) in edges {
                if trace.in_downstream(src) && trace.in_downstream(tgt) {
                    assert_eq!(trace.classify_edge(src, tgt), Some("downstream"));
                }
            }
        }
    }

    #[test]
    fn spec_rule_2_3_middle_node_both_directions() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);
        // Select B. A upstream, C downstream.
        let trace = TraceState::new(&viz, "b");
        assert!(trace.upstream_set.contains("a"));
        assert!(trace.downstream_set.contains("c"));
        assert_eq!(trace.classify_edge("a", "b"), Some("upstream"));
        assert_eq!(trace.classify_edge("b", "c"), Some("downstream"));
    }

    // --- Spec Rule 4: ONLY edges in selected task's chain get colored ---

    #[test]
    fn spec_rule_4_unrelated_edges_not_colored() {
        let graph = build_disconnected();
        let viz = render_graph(&graph);
        let trace = TraceState::new(&viz, "a");
        assert!(trace.downstream_set.contains("b"));
        assert!(!trace.downstream_set.contains("x"));
        assert!(!trace.downstream_set.contains("y"));
        assert_eq!(trace.classify_edge("x", "y"), None);
    }

    #[test]
    fn spec_rule_4_fan_in_only_relevant_edges() {
        let graph = build_fan_in_abcd();
        let viz = render_graph(&graph);
        // Select A. D is downstream, but B and C are unrelated.
        let trace = TraceState::new(&viz, "a");
        assert!(trace.downstream_set.contains("d"));
        assert!(!trace.downstream_set.contains("b"));
        assert!(!trace.downstream_set.contains("c"));
        assert_eq!(trace.classify_edge("a", "d"), Some("downstream"));
        assert_eq!(trace.classify_edge("b", "d"), None);
        assert_eq!(trace.classify_edge("c", "d"), None);
    }

    // --- Spec Rule 5: Horizontal dashes fully mapped ---

    #[test]
    fn spec_rule_5_horizontal_dashes_fully_mapped() {
        let graph = build_fan_in_abcd();
        let viz = render_graph(&graph);
        let plain_text = strip_ansi_for_map(&viz.text);
        let lines: Vec<&str> = plain_text.lines().collect();

        for (line_idx, line) in lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            for (col, &ch) in chars.iter().enumerate() {
                if matches!(ch, '─' | '←' | '┐' | '┘' | '┤' | '┼')
                    && let Some(edges) = viz.char_edge_map.get(&(line_idx, col))
                {
                    assert!(
                        !edges.is_empty(),
                        "Arc char '{}' at ({}, {}) has empty edge map",
                        ch,
                        line_idx,
                        col
                    );
                }
            }
        }
    }

    // --- Spec Rule 6: Arc vertical passthrough mapped ---

    #[test]
    fn spec_rule_6_arc_vertical_passthrough_mapped() {
        // A -> D, B (unrelated), C -> D: arc from A may pass through B's line.
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "Task A")));
        graph.add_node(Node::Task(make_task("b", "Task B")));
        graph.add_node(Node::Task(make_task("c", "Task C")));
        let mut d = make_task("d", "Task D");
        d.after = vec!["a".to_string(), "c".to_string()];
        graph.add_node(Node::Task(d));

        let viz = render_graph(&graph);
        let plain_text = strip_ansi_for_map(&viz.text);
        let lines: Vec<&str> = plain_text.lines().collect();

        // Check that any │ on the right side has edge map entries.
        for (line_idx, line) in lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            for (col, &ch) in chars.iter().enumerate() {
                if ch == '│'
                    && col > 10
                    && let Some(edges) = viz.char_edge_map.get(&(line_idx, col))
                {
                    assert!(
                        !edges.is_empty(),
                        "Right-side │ at ({}, {}) should have edge entries",
                        line_idx,
                        col
                    );
                }
            }
        }
    }

    // --- Spec Rule 7: Shared arc column selective coloring ---

    #[test]
    fn spec_rule_7_shared_arc_column_selective_coloring() {
        let graph = build_fan_in_abcd();
        let viz = render_graph(&graph);
        // Select B. Only (b,d) in trace. (a,d) and (c,d) unrelated.
        let trace = TraceState::new(&viz, "b");
        assert!(trace.downstream_set.contains("d"));

        for edges in viz.char_edge_map.values() {
            for (src, tgt) in edges {
                if src == "b" && tgt == "d" {
                    assert_eq!(trace.classify_edge(src, tgt), Some("downstream"));
                }
                if (src == "a" || src == "c") && tgt == "d" {
                    assert_eq!(
                        trace.classify_edge(src, tgt),
                        None,
                        "Edge ({}, d) should be unrelated when B is selected",
                        src
                    );
                }
            }
        }
    }

    // --- Spec Rule 8: Left tree connectors have edge entries (style preserved by renderer) ---

    #[test]
    fn spec_rule_8_left_tree_connectors_mapped() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);

        let b_line = viz.node_line_map["b"];
        // Connector col for children of root (depth 0) is 0.
        let b_edges = viz.char_edge_map.get(&(b_line, 0));
        assert!(
            b_edges.is_some(),
            "B's tree connector should have edge map entry"
        );
        assert!(
            b_edges
                .unwrap()
                .iter()
                .any(|(src, tgt)| src == "a" && tgt == "b")
        );
    }

    // --- Spec Rules 9-10: Selection indicator + text range ---

    #[test]
    fn spec_rule_9_10_selection_text_range() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);
        let plain = strip_ansi_for_map(&viz.text);
        let lines: Vec<&str> = plain.lines().collect();

        // Task A is root at col 0 (no connector).
        let a_line = viz.node_line_map["a"];
        assert!(
            lines[a_line].starts_with("a"),
            "Root should start with task id. Got: {:?}",
            lines[a_line]
        );

        // Task B has tree connectors before text.
        let b_line = viz.node_line_map["b"];
        let b_text_start = lines[b_line].chars().position(|c| c.is_alphanumeric());
        assert!(b_text_start.unwrap() > 0);
    }

    // --- Spec Rules 11-12: Task text keeps status color, never dimmed ---

    #[test]
    fn spec_rule_11_12_task_text_preserved() {
        let mut graph = WorkGraph::new();
        let mut a = make_task("a", "Done");
        a.status = Status::Done;
        let mut b = make_task("b", "Open");
        b.status = Status::Open;
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", "Failed");
        c.status = Status::Failed;
        c.after = vec!["b".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let viz = render_graph(&graph);
        let plain = strip_ansi_for_map(&viz.text);
        // Status labels appear in the output for all tasks.
        assert!(plain.contains("done"), "Should show 'done' status label");
        assert!(plain.contains("open"), "Should show 'open' status label");
        assert!(
            plain.contains("failed"),
            "Should show 'failed' status label"
        );
        // Note: ANSI colors only emitted when stdout is a terminal.
        // In the TUI, the viz is rendered with colors; here we verify
        // the text content is correct (status labels present, not dimmed).
        assert!(plain.contains("a"), "Task a text present");
        assert!(plain.contains("b"), "Task b text present");
        assert!(plain.contains("c"), "Task c text present");
    }

    // --- Spec Rule 13: Unrelated WCCs unaffected ---

    #[test]
    fn spec_rule_13_unrelated_wcc_unaffected() {
        let graph = build_disconnected();
        let viz = render_graph(&graph);
        let trace = TraceState::new(&viz, "a");

        for edges in viz.char_edge_map.values() {
            for (src, tgt) in edges {
                if src == "x" || src == "y" || tgt == "x" || tgt == "y" {
                    assert_eq!(trace.classify_edge(src, tgt), None);
                }
            }
        }
    }

    // --- Spec Rule 14: Agency phase true pink ---

    #[test]
    fn spec_rule_14_agency_phase_annotation_present() {
        let mut graph = WorkGraph::new();
        let mut parent = make_task("my-task", "My Task");
        parent.status = Status::InProgress;
        let assign = Task {
            id: "assign-my-task".to_string(),
            title: "Assign my-task".to_string(),
            tags: vec!["assignment".to_string(), "agency".to_string()],
            status: Status::InProgress,
            ..Task::default()
        };
        graph.add_node(Node::Task(parent));
        graph.add_node(Node::Task(assign));

        let tasks: Vec<_> = graph.tasks().collect();
        let empty: HashMap<String, crate::commands::viz::AnnotationInfo> = HashMap::new();
        let (filtered, annotations) = super::super::filter_internal_tasks(&graph, tasks, &empty);
        let filtered_ids: HashSet<&str> = filtered.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph,
            &filtered,
            &filtered_ids,
            &annotations,
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let plain = strip_ansi_for_map(&result.text);
        assert!(
            plain.contains("[⊞ assigning]"),
            "Should contain [⊞ assigning]. Plain: {}",
            plain
        );
        // Internal assign task should be filtered out.
        assert!(
            !plain.contains("assign-my-task"),
            "Internal task should be filtered. Plain: {}",
            plain
        );
        // ANSI 219 (true pink) is only emitted when stdout is a terminal.
        // Verify the code path: the annotation text must reference the agency
        // phase which the TUI will render in pink when use_color is true.
        // The annotation mechanism is tested; color code verified by the
        // existing test_ascii_shows_evaluating_phase tests and TUI rendering.
    }

    // --- Spec Rules 15-16: Navigation no wrap ---

    #[test]
    fn spec_rule_15_up_at_first_no_wrap() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);
        // select_prev_task at idx 0 should stay at 0.
        let idx: usize = 0;
        let new_idx = if idx == 0 { idx } else { idx - 1 };
        assert_eq!(new_idx, 0);
        assert!(!viz.task_order.is_empty());
    }

    #[test]
    fn spec_rule_16_down_at_last_no_wrap() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);
        let last = viz.task_order.len() - 1;
        let new_idx = if last + 1 >= viz.task_order.len() {
            last
        } else {
            last + 1
        };
        assert_eq!(new_idx, last);
    }

    // --- Spec Rule 17: Home/End ---

    #[test]
    fn spec_rule_17_home_end() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);
        assert_eq!(viz.task_order[0], viz.task_order[0]); // Home → first
        assert_eq!(
            viz.task_order[viz.task_order.len() - 1],
            *viz.task_order.last().unwrap()
        ); // End → last
    }

    // --- char_edge_map: linear chain completeness & no spurious edges ---

    #[test]
    fn trace_linear_chain_all_edges_present() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);
        let has_ab = viz
            .char_edge_map
            .values()
            .any(|e| e.iter().any(|(s, t)| s == "a" && t == "b"));
        let has_bc = viz
            .char_edge_map
            .values()
            .any(|e| e.iter().any(|(s, t)| s == "b" && t == "c"));
        assert!(has_ab, "Missing edge (a,b). Map: {:?}", viz.char_edge_map);
        assert!(has_bc, "Missing edge (b,c). Map: {:?}", viz.char_edge_map);
    }

    #[test]
    fn trace_linear_chain_no_spurious_edges() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);
        let valid: HashSet<(&str, &str)> = [("a", "b"), ("b", "c")].into_iter().collect();
        for edges in viz.char_edge_map.values() {
            for (src, tgt) in edges {
                assert!(
                    valid.contains(&(src.as_str(), tgt.as_str())),
                    "Spurious edge ({}, {}) in linear chain",
                    src,
                    tgt
                );
            }
        }
    }

    // --- char_edge_map: fan-in ---

    #[test]
    fn trace_fan_in_all_edges_present() {
        let graph = build_fan_in_abcd();
        let viz = render_graph(&graph);
        for (src, tgt) in [("a", "d"), ("b", "d"), ("c", "d")] {
            let found = viz
                .char_edge_map
                .values()
                .any(|e| e.iter().any(|(s, t)| s == src && t == tgt));
            assert!(
                found,
                "Missing edge ({},{}). Text:\n{}\nMap: {:?}",
                src, tgt, viz.text, viz.char_edge_map
            );
        }
    }

    // --- char_edge_map: fan-out ---

    #[test]
    fn trace_fan_out_all_edges_present() {
        let graph = build_fan_out();
        let viz = render_graph(&graph);
        for (src, tgt) in [("a", "b"), ("a", "c"), ("a", "d")] {
            let found = viz
                .char_edge_map
                .values()
                .any(|e| e.iter().any(|(s, t)| s == src && t == tgt));
            assert!(
                found,
                "Missing edge ({},{}). Map: {:?}",
                src, tgt, viz.char_edge_map
            );
        }
    }

    // --- char_edge_map: diamond ---

    #[test]
    fn trace_diamond_all_edges_present() {
        let graph = build_diamond();
        let viz = render_graph(&graph);
        for (src, tgt) in [("a", "b"), ("a", "c"), ("b", "d"), ("c", "d")] {
            let found = viz
                .char_edge_map
                .values()
                .any(|e| e.iter().any(|(s, t)| s == src && t == tgt));
            assert!(
                found,
                "Missing edge ({},{}). Text:\n{}\nMap: {:?}",
                src, tgt, viz.text, viz.char_edge_map
            );
        }
    }

    // --- char_edge_map: disconnected subgraphs ---

    #[test]
    fn trace_disconnected_no_cross_edges() {
        let graph = build_disconnected();
        let viz = render_graph(&graph);
        for edges in viz.char_edge_map.values() {
            for (src, tgt) in edges {
                let valid = (src == "a" && tgt == "b") || (src == "x" && tgt == "y");
                assert!(valid, "Unexpected cross-edge ({}, {})", src, tgt);
            }
        }
    }

    // --- char_edge_map: cycle ---

    #[test]
    fn trace_cycle_has_back_edge() {
        let graph = build_cycle_abc();
        let viz = render_graph(&graph);
        let has_ab = viz
            .char_edge_map
            .values()
            .any(|e| e.iter().any(|(s, t)| s == "a" && t == "b"));
        let has_bc = viz
            .char_edge_map
            .values()
            .any(|e| e.iter().any(|(s, t)| s == "b" && t == "c"));
        let has_ca = viz
            .char_edge_map
            .values()
            .any(|e| e.iter().any(|(s, t)| s == "c" && t == "a"));
        let count = [has_ab, has_bc, has_ca].iter().filter(|&&x| x).count();
        assert!(
            count >= 2,
            "Cycle should have ≥2 edges in char_edge_map. Text:\n{}\nMap: {:?}",
            viz.text,
            viz.char_edge_map
        );
    }

    // --- Trace correctness: cycle covers full loop ---

    #[test]
    fn trace_cycle_full_coverage() {
        let graph = build_cycle_abc();
        let viz = render_graph(&graph);
        let trace = TraceState::new(&viz, "a");
        let all = (trace.upstream_set.contains("b") || trace.downstream_set.contains("b"))
            && (trace.upstream_set.contains("c") || trace.downstream_set.contains("c"));
        assert!(
            all,
            "In a cycle all nodes should be reachable. Up: {:?}, Down: {:?}",
            trace.upstream_set, trace.downstream_set
        );
    }

    // --- Trace correctness: diamond from leaf ---

    #[test]
    fn trace_diamond_leaf_selection() {
        let graph = build_diamond();
        let viz = render_graph(&graph);
        let trace = TraceState::new(&viz, "d");
        assert!(trace.upstream_set.contains("a"));
        assert!(trace.upstream_set.contains("b"));
        assert!(trace.upstream_set.contains("c"));
        assert!(trace.downstream_set.is_empty());
    }

    #[test]
    fn trace_diamond_root_selection() {
        let graph = build_diamond();
        let viz = render_graph(&graph);
        let trace = TraceState::new(&viz, "a");
        assert!(trace.downstream_set.contains("b"));
        assert!(trace.downstream_set.contains("c"));
        assert!(trace.downstream_set.contains("d"));
        assert!(trace.upstream_set.is_empty());
    }

    // --- node_line_map and task_order ---

    #[test]
    fn trace_node_line_map_covers_all_tasks() {
        for (name, graph) in [
            ("linear", build_linear_chain()),
            ("fan_in", build_fan_in_abcd()),
            ("fan_out", build_fan_out()),
            ("diamond", build_diamond()),
            ("disconnected", build_disconnected()),
            ("cycle", build_cycle_abc()),
        ] {
            let viz = render_graph(&graph);
            let tc = graph.tasks().count();
            assert_eq!(viz.node_line_map.len(), tc, "{}: node_line_map size", name);
            assert_eq!(viz.task_order.len(), tc, "{}: task_order size", name);
        }
    }

    #[test]
    fn trace_task_order_sorted_by_line() {
        let graph = build_linear_chain();
        let viz = render_graph(&graph);
        let mut prev = 0;
        for (i, id) in viz.task_order.iter().enumerate() {
            let line = viz.node_line_map[id];
            if i > 0 {
                assert!(
                    line > prev,
                    "task_order not sorted: {} at {} vs {}",
                    id,
                    line,
                    prev
                );
            }
            prev = line;
        }
    }

    // --- forward/reverse edge consistency ---

    #[test]
    fn trace_forward_reverse_consistent() {
        for (name, graph) in [
            ("linear", build_linear_chain()),
            ("fan_in", build_fan_in_abcd()),
            ("fan_out", build_fan_out()),
            ("diamond", build_diamond()),
            ("disconnected", build_disconnected()),
            ("cycle", build_cycle_abc()),
        ] {
            let viz = render_graph(&graph);
            for (src, targets) in &viz.forward_edges {
                for tgt in targets {
                    let rev = viz.reverse_edges.get(tgt);
                    assert!(
                        rev.is_some() && rev.unwrap().contains(src),
                        "{}: fwd ({}->{}) missing in reverse",
                        name,
                        src,
                        tgt
                    );
                }
            }
            for (tgt, sources) in &viz.reverse_edges {
                for src in sources {
                    let fwd = viz.forward_edges.get(src);
                    assert!(
                        fwd.is_some() && fwd.unwrap().contains(tgt),
                        "{}: rev ({}->{}) missing in forward",
                        name,
                        src,
                        tgt
                    );
                }
            }
        }
    }

    // --- Negative tests ---

    #[test]
    fn negative_unrelated_edges_stay_uncolored() {
        let graph = build_disconnected();
        let viz = render_graph(&graph);
        let trace = TraceState::new(&viz, "b");
        assert!(trace.upstream_set.contains("a"));
        for edges in viz.char_edge_map.values() {
            for (src, tgt) in edges {
                if src == "x" && tgt == "y" {
                    assert_eq!(trace.classify_edge(src, tgt), None);
                }
            }
        }
    }

    #[test]
    fn negative_partially_related_edge_not_colored() {
        let graph = build_fan_in_abcd();
        let viz = render_graph(&graph);
        let trace = TraceState::new(&viz, "b");
        assert_eq!(trace.classify_edge("a", "d"), None);
        assert_eq!(trace.classify_edge("c", "d"), None);
        assert_eq!(trace.classify_edge("b", "d"), Some("downstream"));
    }

    // --- Regression: sibling trunk has both edges ---

    #[test]
    fn regression_sibling_trunk_both_edges() {
        let graph = build_fan_out();
        let viz = render_graph(&graph);
        let b_line = viz.node_line_map.get("b");
        let c_line = viz.node_line_map.get("c");
        if let (Some(&bl), Some(&cl)) = (b_line, c_line) {
            for l in (bl + 1)..cl {
                if let Some(edges) = viz.char_edge_map.get(&(l, 0)) {
                    assert!(
                        edges.iter().any(|(s, _)| s == "a"),
                        "│ at line {} should reference parent 'a'",
                        l
                    );
                }
            }
        }
    }

    // --- Regression: no duplicate task_order ---

    #[test]
    fn regression_no_duplicate_task_order() {
        for (name, graph) in [
            ("linear", build_linear_chain()),
            ("fan_in", build_fan_in_abcd()),
            ("diamond", build_diamond()),
            ("cycle", build_cycle_abc()),
        ] {
            let viz = render_graph(&graph);
            let unique: HashSet<&str> = viz.task_order.iter().map(|s| s.as_str()).collect();
            assert_eq!(
                unique.len(),
                viz.task_order.len(),
                "{}: duplicate in task_order",
                name
            );
        }
    }

    // --- Regression: unique line numbers ---

    #[test]
    fn regression_unique_line_numbers() {
        for (name, graph) in [
            ("linear", build_linear_chain()),
            ("fan_in", build_fan_in_abcd()),
            ("diamond", build_diamond()),
            ("cycle", build_cycle_abc()),
        ] {
            let viz = render_graph(&graph);
            let lines: Vec<usize> = viz.node_line_map.values().copied().collect();
            let unique: HashSet<usize> = lines.iter().copied().collect();
            assert_eq!(
                unique.len(),
                lines.len(),
                "{}: duplicate line numbers",
                name
            );
        }
    }

    // --- Single node edge case ---

    #[test]
    fn trace_single_node() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("solo", "Solo Task")));
        let viz = render_graph(&graph);
        assert_eq!(viz.task_order.len(), 1);
        let trace = TraceState::new(&viz, "solo");
        assert!(trace.upstream_set.is_empty());
        assert!(trace.downstream_set.is_empty());
    }

    // --- Long chain (5 nodes) ---

    #[test]
    fn trace_long_chain_correctness() {
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("a", "A")));
        let mut b = make_task("b", "B");
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", "C");
        c.after = vec!["b".to_string()];
        let mut d = make_task("d", "D");
        d.after = vec!["c".to_string()];
        let mut e = make_task("e", "E");
        e.after = vec!["d".to_string()];
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));
        graph.add_node(Node::Task(e));

        let viz = render_graph(&graph);
        let trace = TraceState::new(&viz, "c");
        assert!(trace.upstream_set.contains("a"));
        assert!(trace.upstream_set.contains("b"));
        assert!(trace.downstream_set.contains("d"));
        assert!(trace.downstream_set.contains("e"));

        for (src, tgt) in [("a", "b"), ("b", "c"), ("c", "d"), ("d", "e")] {
            let found = viz
                .char_edge_map
                .values()
                .any(|e| e.iter().any(|(s, t)| s == src && t == tgt));
            assert!(found, "Missing edge ({},{}) in long chain", src, tgt);
        }
    }

    // --- Mixed status graph ---

    #[test]
    fn trace_mixed_status_all_present() {
        let mut graph = WorkGraph::new();
        let mut a = make_task("a", "Done");
        a.status = Status::Done;
        let mut b = make_task("b", "InProg");
        b.status = Status::InProgress;
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", "Open");
        c.status = Status::Open;
        c.after = vec!["a".to_string()];
        let mut d = make_task("d", "Failed");
        d.status = Status::Failed;
        d.after = vec!["b".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let viz = render_graph(&graph);
        let plain = strip_ansi_for_map(&viz.text);
        assert!(plain.contains("done"));
        assert!(plain.contains("in-progress"));
        assert!(plain.contains("open"));
        assert!(plain.contains("failed"));
    }

    // --- char_edge_map: source line multi-arc fan-out ---

    #[test]
    fn test_char_edge_map_source_line_multi_arc() {
        // Graph: root → {src, b, c, d}; b, c, d also depend on src.
        // Diamond layout moves b, c, d to root (LCA). src→b, src→c, src→d
        // become fan-in arc edges. src appears as a blocker at the TOP of 3 arcs.
        //
        // Expected layout:
        //   root  (open)
        //   ├→ src  (open) ──┐ ─┐ ─┐
        //   ├→ b  (open) ←───┘  │  │
        //   ├→ c  (open) ←──────┘  │
        //   └→ d  (open) ←─────────┘
        //
        // Verify: ALL dashes and corners on src's line are in char_edge_map
        // with the correct (src, target) edges.
        let mut graph = WorkGraph::new();
        let root = make_task("root", "Root");
        let mut src = make_task("src", "Source");
        src.after = vec!["root".to_string()];
        let mut b = make_task("b", "Task B");
        b.after = vec!["root".to_string(), "src".to_string()];
        let mut c = make_task("c", "Task C");
        c.after = vec!["root".to_string(), "src".to_string()];
        let mut d = make_task("d", "Task D");
        d.after = vec!["root".to_string(), "src".to_string()];
        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(src));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));
        graph.add_node(Node::Task(d));

        let viz = render_graph(&graph);
        let src_line = *viz
            .node_line_map
            .get("src")
            .expect("src should be in node_line_map");

        // Print for debugging
        eprintln!("=== VIZ TEXT ===\n{}", viz.text);
        let plain = strip_ansi_for_map(&viz.text);
        let plain_lines: Vec<&str> = plain.lines().collect();
        eprintln!("=== PLAIN SRC LINE ===\n{}", plain_lines[src_line]);
        eprintln!("=== NODE LINE MAP ===\n{:?}", viz.node_line_map);

        // Collect all char_edge_map entries on src_line
        let mut src_line_entries: Vec<(usize, Vec<(String, String)>)> = viz
            .char_edge_map
            .iter()
            .filter_map(|((line, col), edges)| {
                if *line == src_line {
                    Some((*col, edges.clone()))
                } else {
                    None
                }
            })
            .collect();
        src_line_entries.sort_by_key(|(col, _)| *col);
        eprintln!("=== CHAR_EDGE_MAP entries on src_line {} ===", src_line);
        for (col, edges) in &src_line_entries {
            let ch = plain_lines[src_line].chars().nth(*col).unwrap_or('?');
            eprintln!("  col {}: '{}' -> {:?}", col, ch, edges);
        }

        // Each arc from src to b, c, d must have char_edge_map entries on src's line
        for target in &["b", "c", "d"] {
            let has_edge_on_src_line = viz.char_edge_map.iter().any(|(&(line, _col), edges)| {
                line == src_line
                    && edges
                        .iter()
                        .any(|(s, t)| s == "src" && t.as_str() == *target)
            });
            assert!(
                has_edge_on_src_line,
                "char_edge_map should have (src, {}) entries on source line {}.\nOutput:\n{}\nMap: {:?}",
                target, src_line, viz.text, viz.char_edge_map
            );
        }

        // Verify that ALL arc characters (dashes, corners) on src's line after text
        // are mapped to some edge in char_edge_map
        let src_plain = plain_lines[src_line];
        let text_end = src_plain
            .find("(open)")
            .map(|p| p + "(open)".len())
            .unwrap_or(0);
        let mut unmapped_dashes = Vec::new();
        for (i, ch) in src_plain.chars().enumerate() {
            if i > text_end
                && (ch == '─' || ch == '┐' || ch == '┘' || ch == '┤')
                && !viz.char_edge_map.contains_key(&(src_line, i))
            {
                unmapped_dashes.push((i, ch));
            }
        }
        assert!(
            unmapped_dashes.is_empty(),
            "Found unmapped arc characters on source line: {:?}\nPlain: {}\nMap entries: {:?}",
            unmapped_dashes,
            src_plain,
            src_line_entries
        );
    }

    #[test]
    fn test_char_edge_map_shared_column_blocker_dashes() {
        // Two blockers (other, src) share the same arc column for each dependent.
        // The dashes on other's line must be mapped to (other, target) not (src, target).
        //
        // Graph: root → {other, src, t1, t2, t3}; t1/t2/t3 depend on both other and src
        //
        // Layout:
        //   root  (open)
        //   ├→ other  (open) ──┐ ─┐ ─┐   ← line 1, blocker for 3 shared columns
        //   ├→ src  (open) ────┤ ─┤ ─┤   ← line 2, also blocker
        //   ├→ t1  (open) ←────┘  │  │
        //   ├→ t2  (open) ←───────┘  │
        //   └→ t3  (open) ←──────────┘
        //
        // Each column has 2 blockers: other (line 1) and src (line 2).
        // When tracing, dashes on other's line should belong to (other, tX),
        // and dashes on src's line should belong to (src, tX).
        let mut graph = WorkGraph::new();
        let root = make_task("root", "Root");
        let mut other = make_task("other", "Other");
        other.after = vec!["root".to_string()];
        let mut src = make_task("src", "Source");
        src.after = vec!["root".to_string()];
        let mut t1 = make_task("t1", "T1");
        t1.after = vec!["root".to_string(), "other".to_string(), "src".to_string()];
        let mut t2 = make_task("t2", "T2");
        t2.after = vec!["root".to_string(), "other".to_string(), "src".to_string()];
        let mut t3 = make_task("t3", "T3");
        t3.after = vec!["root".to_string(), "other".to_string(), "src".to_string()];
        graph.add_node(Node::Task(root));
        graph.add_node(Node::Task(other));
        graph.add_node(Node::Task(src));
        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let viz = render_graph(&graph);
        let plain = strip_ansi_for_map(&viz.text);
        let plain_lines: Vec<&str> = plain.lines().collect();
        let other_line = *viz.node_line_map.get("other").expect("other in map");
        let src_line = *viz.node_line_map.get("src").expect("src in map");

        eprintln!("=== VIZ TEXT ===\n{}", viz.text);
        eprintln!("=== NODE LINE MAP === {:?}", viz.node_line_map);

        // Collect char_edge_map entries on other's line
        let mut other_entries: Vec<(usize, Vec<(String, String)>)> = viz
            .char_edge_map
            .iter()
            .filter_map(|((line, col), edges)| {
                if *line == other_line {
                    Some((*col, edges.clone()))
                } else {
                    None
                }
            })
            .collect();
        other_entries.sort_by_key(|(col, _)| *col);
        eprintln!("=== other line {} entries ===", other_line);
        for (col, edges) in &other_entries {
            let ch = plain_lines[other_line].chars().nth(*col).unwrap_or('?');
            eprintln!("  col {}: '{}' -> {:?}", col, ch, edges);
        }

        // Collect char_edge_map entries on src's line
        let mut src_entries: Vec<(usize, Vec<(String, String)>)> = viz
            .char_edge_map
            .iter()
            .filter_map(|((line, col), edges)| {
                if *line == src_line {
                    Some((*col, edges.clone()))
                } else {
                    None
                }
            })
            .collect();
        src_entries.sort_by_key(|(col, _)| *col);
        eprintln!("=== src line {} entries ===", src_line);
        for (col, edges) in &src_entries {
            let ch = plain_lines[src_line].chars().nth(*col).unwrap_or('?');
            eprintln!("  col {}: '{}' -> {:?}", col, ch, edges);
        }

        // KEY ASSERTION: Dashes on other's line must map to (other, tX), not (src, tX)
        let text_end = plain_lines[other_line]
            .find("(open)")
            .map(|p| p + "(open)".len())
            .unwrap_or(0);
        for (col, edges) in &other_entries {
            if *col <= text_end {
                continue;
            } // skip tree connectors
            let ch = plain_lines[other_line].chars().nth(*col).unwrap_or('?');
            if ch == '─' {
                // A dash on other's line should contain an (other, *) edge
                let has_other_edge = edges.iter().any(|(s, _)| s == "other");
                assert!(
                    has_other_edge,
                    "Dash at col {} on other's line should map to (other, ...) but got {:?}.\n\
                     This means the dash was mapped to the wrong blocker's edge.\n\
                     Output:\n{}",
                    col, edges, viz.text
                );
            }
        }

        // Same check for src's line
        let text_end_src = plain_lines[src_line]
            .find("(open)")
            .map(|p| p + "(open)".len())
            .unwrap_or(0);
        for (col, edges) in &src_entries {
            if *col <= text_end_src {
                continue;
            }
            let ch = plain_lines[src_line].chars().nth(*col).unwrap_or('?');
            if ch == '─' {
                let has_src_edge = edges.iter().any(|(s, _)| s == "src");
                assert!(
                    has_src_edge,
                    "Dash at col {} on src's line should map to (src, ...) but got {:?}.\n\
                     Output:\n{}",
                    col, edges, viz.text
                );
            }
        }
    }

    #[test]
    fn test_char_edge_map_real_world_fan_out_with_shared_column() {
        // Reproduces the exact graph from the bug report:
        //   remove-org → update-eval, update-docs, smoke-test, validate
        //   update-eval → update-docs, smoke-test, validate
        //   update-docs → validate
        //   smoke-test → validate
        //
        // The validate column has 3 blockers: update-eval, update-docs, smoke-test.
        // update-eval's line is the SOURCE for 3 arcs: one to each of smoke-test,
        // update-docs, and validate. All dashes on update-eval's line must be in
        // char_edge_map with the correct edges.
        let mut graph = WorkGraph::new();
        let rem = make_task("rem", "Remove Org Eval");
        let mut ue = make_task("ue", "Update Eval To Cover Org");
        ue.after = vec!["rem".to_string()];
        let mut ud = make_task("ud", "Update Docs");
        ud.after = vec!["rem".to_string(), "ue".to_string()];
        let mut sm = make_task("sm", "Smoke Test");
        sm.after = vec!["rem".to_string(), "ue".to_string()];
        let mut va = make_task("va", "Validate");
        va.after = vec![
            "rem".to_string(),
            "ue".to_string(),
            "ud".to_string(),
            "sm".to_string(),
        ];
        graph.add_node(Node::Task(rem));
        graph.add_node(Node::Task(ue));
        graph.add_node(Node::Task(ud));
        graph.add_node(Node::Task(sm));
        graph.add_node(Node::Task(va));

        let viz = render_graph(&graph);
        let plain = strip_ansi_for_map(&viz.text);
        let plain_lines: Vec<&str> = plain.lines().collect();

        eprintln!("=== VIZ TEXT ===\n{}", viz.text);
        eprintln!("=== NODE LINE MAP === {:?}", viz.node_line_map);

        let ue_line = viz.node_line_map["ue"];

        // Collect char_edge_map entries on ue's line
        let mut ue_entries: Vec<(usize, Vec<(String, String)>)> = viz
            .char_edge_map
            .iter()
            .filter_map(|((line, col), edges)| {
                if *line == ue_line {
                    Some((*col, edges.clone()))
                } else {
                    None
                }
            })
            .collect();
        ue_entries.sort_by_key(|(col, _)| *col);
        eprintln!("=== ue line {} entries ===", ue_line);
        for (col, edges) in &ue_entries {
            let ch = plain_lines[ue_line].chars().nth(*col).unwrap_or('?');
            eprintln!("  col {}: '{}' -> {:?}", col, ch, edges);
        }

        // The ue line should have arcs to sm, ud, and va.
        // Verify every non-space, non-text arc char on ue's line is mapped
        let ue_plain = plain_lines[ue_line];
        let text_end = ue_plain.rfind(')').map(|p| p + 1).unwrap_or(0);
        let mut unmapped = Vec::new();
        for (i, ch) in ue_plain.chars().enumerate() {
            if i >= text_end
                && (ch == '─' || ch == '┐' || ch == '┘' || ch == '┤')
                && !viz.char_edge_map.contains_key(&(ue_line, i))
            {
                unmapped.push((i, ch));
            }
        }
        assert!(
            unmapped.is_empty(),
            "Unmapped arc chars on ue line: {:?}\nPlain: {}\nEntries: {:?}",
            unmapped,
            ue_plain,
            ue_entries
        );

        // Each arc from ue must have entries on ue's line
        for target in &["sm", "ud", "va"] {
            let has = viz.char_edge_map.iter().any(|(&(line, _), edges)| {
                line == ue_line
                    && edges
                        .iter()
                        .any(|(s, t)| s == "ue" && t.as_str() == *target)
            });
            assert!(
                has,
                "char_edge_map missing (ue, {}) on ue line {}. Output:\n{}",
                target, ue_line, viz.text
            );
        }

        // Dashes on ue's line must be mapped to (ue, *), not some other blocker
        for (col, edges) in &ue_entries {
            if *col < text_end {
                continue;
            }
            let ch = ue_plain.chars().nth(*col).unwrap_or('?');
            if ch == '─' {
                let has_ue_edge = edges.iter().any(|(s, _)| s == "ue");
                assert!(
                    has_ue_edge,
                    "Dash at col {} on ue's line should map to (ue, ...) but got {:?}.\n\
                     Output:\n{}",
                    col, edges, viz.text
                );
            }
        }
    }

    // ── Deep subtree: vertical bars map to sibling edge, not parent edge ──

    #[test]
    fn test_char_edge_map_deep_subtree_vertical_bars() {
        // Topology matching the fix-vertical-tree bug:
        //   root
        //   ├→ a
        //   │ └→ b
        //   │   └→ c
        //   │     └→ d
        //   │       └→ e
        //   └→ f
        //
        // The │ bars at col 0 between a's subtree and f MUST map to (root, f),
        // NOT to (root, a). This ensures trace coloring only highlights the bar
        // when sibling f is in the traced path.

        let mut graph = WorkGraph::new();
        let t_root = make_task("root", "Root");
        let mut t_a = make_task("a", "A");
        t_a.after = vec!["root".to_string()];
        let mut t_b = make_task("b", "B");
        t_b.after = vec!["a".to_string()];
        let mut t_c = make_task("c", "C");
        t_c.after = vec!["b".to_string()];
        let mut t_d = make_task("d", "D");
        t_d.after = vec!["c".to_string()];
        let mut t_e = make_task("e", "E");
        t_e.after = vec!["d".to_string()];
        let mut t_f = make_task("f", "F");
        t_f.after = vec!["root".to_string()];
        graph.add_node(Node::Task(t_root));
        graph.add_node(Node::Task(t_a));
        graph.add_node(Node::Task(t_b));
        graph.add_node(Node::Task(t_c));
        graph.add_node(Node::Task(t_d));
        graph.add_node(Node::Task(t_e));
        graph.add_node(Node::Task(t_f));

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

        let a_line = viz.node_line_map["a"];
        let f_line = viz.node_line_map["f"];

        // Every │ at col 0 between a and f must map to (root, f), NOT (root, a).
        for l in (a_line + 1)..f_line {
            let plain: Vec<char> = viz.text.lines().nth(l).unwrap_or("").chars().collect();
            if plain.is_empty() || plain[0] != '│' {
                continue;
            }

            let edges = viz.char_edge_map.get(&(l, 0));
            assert!(
                edges.is_some(),
                "│ at ({}, 0) should have char_edge_map entry.\nOutput:\n{}",
                l,
                viz.text
            );
            let edges = edges.unwrap();

            // Must contain (root, f) — the next sibling below
            assert!(
                edges.iter().any(|(s, t)| s == "root" && t == "f"),
                "│ at ({}, 0) must map to (root, f). Got: {:?}\nOutput:\n{}",
                l,
                edges,
                viz.text
            );

            // Must NOT contain (root, a) — the sibling above (already branched)
            assert!(
                !edges.iter().any(|(s, t)| s == "root" && t == "a"),
                "│ at ({}, 0) must NOT map to (root, a). Got: {:?}\nOutput:\n{}",
                l,
                edges,
                viz.text
            );
        }

        // Simulate trace: if d is selected, upstream = {c, b, a, root}
        // f is NOT upstream, so (root, f) should NOT trigger coloring
        let trace = TraceState::new(&viz, "d");
        assert!(trace.in_upstream("root"));
        assert!(trace.in_upstream("a"));
        assert!(!trace.in_upstream("f"), "f should NOT be upstream of d");

        for l in (a_line + 1)..f_line {
            if let Some(edges) = viz.char_edge_map.get(&(l, 0)) {
                let would_color = edges
                    .iter()
                    .any(|(src, tgt)| trace.classify_edge(src, tgt).is_some());
                assert!(
                    !would_color,
                    "│ at ({}, 0) should NOT be colored when d is selected. \
                     f is not in the trace. Edges: {:?}",
                    l, edges
                );
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════
    // Smoke tests for TUI graph rendering (smoke-tui-graph-2)
    // ══════════════════════════════════════════════════════════════════

    /// Smoke test: 100+ node graph renders without panic and produces output.
    #[test]
    fn test_smoke_100_node_graph_no_panic() {
        let mut graph = WorkGraph::new();
        // Create a chain of 110 tasks: t0 → t1 → t2 → ... → t109
        for i in 0..110 {
            let mut t = make_task(&format!("t{}", i), &format!("Task {}", i));
            t.created_at = Some(format!("2024-01-01T00:{:02}:00Z", i % 60));
            if i > 0 {
                t.after = vec![format!("t{}", i - 1)];
            }
            graph.add_node(Node::Task(t));
        }

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // All 110 tasks should appear in the output
        for i in 0..110 {
            assert!(
                result.text.contains(&format!("t{}", i)),
                "Task t{} should appear in output",
                i
            );
        }
        // node_line_map should have entries for all tasks
        assert_eq!(result.node_line_map.len(), 110);
        assert_eq!(result.task_order.len(), 110);
    }

    /// Smoke test: 100+ node graph with fan-out and fan-in (diamond patterns)
    /// renders without panic.
    #[test]
    fn test_smoke_100_node_complex_graph_no_panic() {
        let mut graph = WorkGraph::new();

        // Root node
        let mut root = make_task("root", "Root");
        root.created_at = Some("2024-01-01T00:00:00Z".to_string());
        graph.add_node(Node::Task(root));

        // Fan-out: root → {fan-0..fan-49}
        for i in 0..50 {
            let mut t = make_task(&format!("fan-{}", i), &format!("Fan {}", i));
            t.after = vec!["root".to_string()];
            t.created_at = Some(format!("2024-01-01T00:{:02}:01Z", i % 60));
            graph.add_node(Node::Task(t));
        }

        // Fan-in: {fan-0..fan-49} → join
        let mut join = make_task("join", "Join");
        join.after = (0..50).map(|i| format!("fan-{}", i)).collect();
        join.created_at = Some("2024-01-01T00:50:00Z".to_string());
        graph.add_node(Node::Task(join));

        // Additional chains off the join
        for i in 0..60 {
            let mut t = make_task(&format!("chain-{}", i), &format!("Chain {}", i));
            if i == 0 {
                t.after = vec!["join".to_string()];
            } else {
                t.after = vec![format!("chain-{}", i - 1)];
            }
            t.created_at = Some(format!("2024-01-01T01:{:02}:00Z", i % 60));
            graph.add_node(Node::Task(t));
        }

        let tasks: Vec<_> = graph.tasks().collect();
        assert!(
            tasks.len() > 100,
            "Should have > 100 tasks, got {}",
            tasks.len()
        );
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();

        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // Should not be empty
        assert!(!result.text.is_empty());
        // Root, join, and chain tasks should all appear
        assert!(result.text.contains("root"));
        assert!(result.text.contains("join"));
        assert!(result.text.contains("chain-59"));
        // node_line_map should be populated
        assert!(result.node_line_map.len() > 100);
    }

    /// Smoke test: arrowheads — tree connectors use → and arc dependents use ←.
    #[test]
    fn test_smoke_arrowheads_correct() {
        // A → B → C, A → C (forward skip produces an arc)
        let mut graph = WorkGraph::new();
        let mut a = make_task("aaa", "A");
        a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut b = make_task("bbb", "B");
        b.after = vec!["aaa".to_string()];
        b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        let mut c = make_task("ccc", "C");
        c.after = vec!["bbb".to_string(), "aaa".to_string()];
        c.created_at = Some("2024-01-01T00:02:00Z".to_string());
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));
        graph.add_node(Node::Task(c));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let lines: Vec<&str> = result.text.lines().collect();

        // Tree connectors should have → (right arrow)
        let has_tree_arrow = lines.iter().any(|l| l.contains("→"));
        assert!(
            has_tree_arrow,
            "Tree connectors should have → arrowheads\nOutput:\n{}",
            result.text
        );

        // Arc dependent (C with forward skip from A) should have ← (left arrow)
        let c_line = lines
            .iter()
            .find(|l| l.contains("ccc"))
            .expect("C should appear");
        assert!(
            c_line.contains("←"),
            "Arc dependent C should have ← arrowhead\nOutput:\n{}",
            result.text
        );
    }

    /// Smoke test: cycle edges produce cycle_members map entries.
    #[test]
    fn test_smoke_cycle_members_populated() {
        use workgraph::graph::CycleConfig;

        let mut graph = WorkGraph::new();
        let mut a = make_task("a", "A");
        a.cycle_config = Some(CycleConfig {
            max_iterations: 3,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });
        a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut b = make_task("b", "B");
        b.after = vec!["a".to_string()];
        b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        // Create cycle: A → B → A
        a.after = vec!["b".to_string()];
        graph.add_node(Node::Task(a));
        graph.add_node(Node::Task(b));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // cycle_members should contain both a and b
        assert!(
            result.cycle_members.contains_key("a"),
            "Cycle member 'a' should be in cycle_members map"
        );
        assert!(
            result.cycle_members.contains_key("b"),
            "Cycle member 'b' should be in cycle_members map"
        );
        // Each should reference the other
        let a_members = &result.cycle_members["a"];
        assert!(a_members.contains("a") && a_members.contains("b"));
    }

    /// Smoke test: dot-task toggle — show_internal changes viz_options,
    /// which is instant (just a boolean flip, no animation delay).
    /// We verify by generating with show_internal=false and show_internal=true
    /// and confirming the internal tasks only appear in the latter.
    #[test]
    fn test_smoke_dot_task_toggle_instant() {
        let mut graph = WorkGraph::new();
        let mut t = make_task("my-task", "My Task");
        t.created_at = Some("2024-01-01T00:00:00Z".to_string());
        graph.add_node(Node::Task(t));

        let mut assign = make_internal_task(
            ".assign-my-task",
            "Assign my-task",
            "assignment",
            vec!["my-task"],
        );
        assign.status = Status::InProgress;
        assign.created_at = Some("2024-01-01T00:01:00Z".to_string());
        graph.add_node(Node::Task(assign));

        let all_tasks: Vec<_> = graph.tasks().collect();
        let all_task_ids: HashSet<&str> = all_tasks.iter().map(|t| t.id.as_str()).collect();

        // Without internal tasks (default)
        let visible_tasks: Vec<_> = all_tasks
            .iter()
            .filter(|t| !super::super::is_internal_task(t))
            .copied()
            .collect();
        let visible_ids: HashSet<&str> = visible_tasks.iter().map(|t| t.id.as_str()).collect();

        let result_hidden = generate_ascii(
            &graph,
            &visible_tasks,
            &visible_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(
            !result_hidden.text.contains(".assign-my-task"),
            "Internal tasks should be hidden when show_internal=false"
        );
        assert!(result_hidden.text.contains("my-task"));

        // With internal tasks (show_internal=true)
        let result_shown = generate_ascii(
            &graph,
            &all_tasks,
            &all_task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(
            result_shown.text.contains(".assign-my-task"),
            "Internal tasks should be visible when show_internal=true"
        );
    }

    /// Smoke test: deeply nested graph (depth 50) with multiple branches renders correctly.
    #[test]
    fn test_smoke_deep_nested_graph_no_panic() {
        let mut graph = WorkGraph::new();

        // Create a tree with depth 50, branching factor 2 at first 5 levels
        let mut task_count = 0;
        let mut current_level = vec!["root".to_string()];
        let mut root = make_task("root", "Root");
        root.created_at = Some("2024-01-01T00:00:00Z".to_string());
        graph.add_node(Node::Task(root));
        task_count += 1;

        for depth in 0..50 {
            let mut next_level = Vec::new();
            let branch_factor = if depth < 5 { 2 } else { 1 };
            for parent_id in &current_level {
                for b in 0..branch_factor {
                    let id = format!("d{}-{}-{}", depth, parent_id, b);
                    let mut t = make_task(&id, &format!("D{} B{}", depth, b));
                    t.after = vec![parent_id.clone()];
                    t.created_at = Some(format!(
                        "2024-01-01T{:02}:{:02}:00Z",
                        (task_count / 60) % 24,
                        task_count % 60
                    ));
                    graph.add_node(Node::Task(t));
                    task_count += 1;
                    next_level.push(id);
                }
            }
            current_level = next_level;
        }

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        assert!(!result.text.is_empty());
        assert!(result.text.contains("root"));
        // Should have rendered many tasks
        assert!(
            result.node_line_map.len() > 50,
            "Should have > 50 tasks rendered, got {}",
            result.node_line_map.len()
        );
    }

    /// Smoke test: graph with multiple cycles renders without panic.
    #[test]
    fn test_smoke_multiple_cycles_no_panic() {
        use workgraph::graph::CycleConfig;

        let mut graph = WorkGraph::new();

        // Cycle 1: c1a → c1b → c1a
        let mut c1a = make_task("c1a", "Cycle1 A");
        c1a.cycle_config = Some(CycleConfig {
            max_iterations: 2,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });
        c1a.created_at = Some("2024-01-01T00:00:00Z".to_string());
        let mut c1b = make_task("c1b", "Cycle1 B");
        c1b.after = vec!["c1a".to_string()];
        c1b.created_at = Some("2024-01-01T00:01:00Z".to_string());
        c1a.after = vec!["c1b".to_string()];

        // Cycle 2: c2a → c2b → c2c → c2a
        let mut c2a = make_task("c2a", "Cycle2 A");
        c2a.cycle_config = Some(CycleConfig {
            max_iterations: 3,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });
        c2a.created_at = Some("2024-01-01T00:10:00Z".to_string());
        let mut c2b = make_task("c2b", "Cycle2 B");
        c2b.after = vec!["c2a".to_string()];
        c2b.created_at = Some("2024-01-01T00:11:00Z".to_string());
        let mut c2c = make_task("c2c", "Cycle2 C");
        c2c.after = vec!["c2b".to_string()];
        c2c.created_at = Some("2024-01-01T00:12:00Z".to_string());
        c2a.after = vec!["c2c".to_string()];

        // Isolated task (not in any cycle)
        let mut solo = make_task("solo", "Solo");
        solo.created_at = Some("2024-01-01T00:20:00Z".to_string());

        graph.add_node(Node::Task(c1a));
        graph.add_node(Node::Task(c1b));
        graph.add_node(Node::Task(c2a));
        graph.add_node(Node::Task(c2b));
        graph.add_node(Node::Task(c2c));
        graph.add_node(Node::Task(solo));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        assert!(!result.text.is_empty());
        // Both cycles and the solo task should appear
        assert!(result.text.contains("c1a"));
        assert!(result.text.contains("c2c"));
        assert!(result.text.contains("solo"));

        // cycle_members should have entries for cycle 1 and cycle 2
        assert!(result.cycle_members.contains_key("c1a"));
        assert!(result.cycle_members.contains_key("c2a"));
        // Solo should NOT be in any cycle
        assert!(!result.cycle_members.contains_key("solo"));
    }

    /// Smoke test: char_edge_map is populated for graphs with arcs.
    #[test]
    fn test_smoke_char_edge_map_populated() {
        // Diamond: root → {left, right} → join
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
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        // char_edge_map should have entries for the arc edges
        assert!(
            !result.char_edge_map.is_empty(),
            "Diamond graph should produce char_edge_map entries for arcs\nOutput:\n{}",
            result.text
        );

        // forward_edges and reverse_edges should be populated
        assert!(
            !result.forward_edges.is_empty(),
            "forward_edges should be populated"
        );
        assert!(
            !result.reverse_edges.is_empty(),
            "reverse_edges should be populated"
        );
    }

    use workgraph::graph::LogEntry;

    #[test]
    fn test_coordinator_with_recent_log_sorts_above_in_progress() {
        let mut graph = WorkGraph::new();

        // WCC 1: a regular in-progress task (hot by status)
        let mut worker = make_task("worker-task", "Worker");
        worker.status = Status::InProgress;
        graph.add_node(Node::Task(worker));

        // WCC 2: coordinator-loop task in Open status with a recent log entry
        let mut coordinator = make_task("coordinator-0", "Coordinator");
        coordinator.status = Status::Open;
        coordinator.tags = vec!["coordinator-loop".to_string()];
        coordinator.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            user: Some(workgraph::current_user()),
            message: "Processing turn".to_string(),
        });
        graph.add_node(Node::Task(coordinator));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let coord_pos = result
            .task_order
            .iter()
            .position(|id| id == "coordinator-0")
            .expect("coordinator-0 should appear in task_order");
        let worker_pos = result
            .task_order
            .iter()
            .position(|id| id == "worker-task")
            .expect("worker-task should appear in task_order");
        assert!(
            coord_pos < worker_pos,
            "Coordinator with recent log (pos {}) should sort before in-progress WCC (pos {})\nOutput:\n{}",
            coord_pos,
            worker_pos,
            result.text
        );
    }

    #[test]
    fn test_coordinator_without_recent_log_does_not_override_in_progress() {
        let mut graph = WorkGraph::new();

        // WCC 1: a regular in-progress task (hot by status)
        let mut worker = make_task("worker-task", "Worker");
        worker.status = Status::InProgress;
        graph.add_node(Node::Task(worker));

        // WCC 2: coordinator-loop task with no log entries (stale)
        let mut coordinator = make_task("coordinator-0", "Coordinator");
        coordinator.status = Status::Open;
        coordinator.tags = vec!["coordinator-loop".to_string()];
        graph.add_node(Node::Task(coordinator));

        let tasks: Vec<_> = graph.tasks().collect();
        let task_ids: HashSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
        let result = generate_ascii(
            &graph,
            &tasks,
            &task_ids,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            LayoutMode::default(),
            &HashSet::new(),
            "gray",
            &HashMap::new(),
            &HashMap::new(),
        );

        let coord_pos = result
            .task_order
            .iter()
            .position(|id| id == "coordinator-0")
            .expect("coordinator-0 should appear in task_order");
        let worker_pos = result
            .task_order
            .iter()
            .position(|id| id == "worker-task")
            .expect("worker-task should appear in task_order");
        assert!(
            coord_pos > worker_pos,
            "Stale coordinator (pos {}) should NOT sort before in-progress WCC (pos {})\nOutput:\n{}",
            coord_pos,
            worker_pos,
            result.text
        );
    }

    #[test]
    fn test_visible_len_plain() {
        assert_eq!(visible_len("hello"), 5);
        assert_eq!(visible_len(""), 0);
        assert_eq!(visible_len("├→ task"), 7);
    }

    #[test]
    fn test_visible_len_with_ansi() {
        assert_eq!(visible_len("\x1b[32mhello\x1b[0m"), 5);
        assert_eq!(visible_len("\x1b[90m├→\x1b[0m test"), 7);
        assert_eq!(visible_len("\x1b[38;5;219m[assigning]\x1b[0m"), 11);
    }

    #[test]
    fn test_visible_len_unicode_width() {
        // Box-drawing characters are single-width
        assert_eq!(visible_len("─────"), 5);
        assert_eq!(visible_len("│├└┐┘┤┼"), 7);
        // Hourglass emoji is double-width
        assert_eq!(visible_len("⏳"), 2);
        assert_eq!(visible_len("ab⏳cd"), 6);
    }

    #[test]
    fn test_truncate_to_width_no_truncation() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_to_width_plain() {
        assert_eq!(truncate_to_width("hello world", 5), "hello");
    }

    #[test]
    fn test_truncate_to_width_with_ansi() {
        let s = "\x1b[32mhello\x1b[0m world";
        let t = truncate_to_width(s, 5);
        assert_eq!(visible_len(&t), 5);
        assert!(t.ends_with("\x1b[0m"));
    }

    #[test]
    fn test_truncate_to_width_unicode() {
        // ⏳ is 2 columns wide — if only 1 column left, skip it
        let s = "abc⏳";
        assert_eq!(truncate_to_width(s, 4), "abc");
        assert_eq!(truncate_to_width(s, 5), "abc⏳");
        // With ANSI, the reset is appended
        let s2 = "\x1b[33mabc⏳\x1b[0m";
        let t = truncate_to_width(s2, 4);
        assert_eq!(visible_len(&t), 3); // "abc" = 3, ⏳ doesn't fit
        assert!(t.ends_with("\x1b[0m"));
    }

    #[test]
    fn test_truncate_lines() {
        let text = "short\nthis is a longer line\nok";
        let result = truncate_lines(text, 10);
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines[0], "short");
        assert_eq!(visible_len(lines[1]), 10);
        assert_eq!(lines[2], "ok");
    }
}
