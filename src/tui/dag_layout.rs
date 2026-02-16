/// Graph layout engine using the ascii-dag crate.
///
/// This module uses ascii-dag for the Sugiyama layout algorithm and produces
/// a layout suitable for rendering with Unicode box-drawing characters in a
/// terminal via ratatui.
///
/// The ascii-dag crate handles:
/// - Layer assignment (topological depth)
/// - Crossing minimization (median heuristic)
/// - Coordinate assignment
/// - Edge routing (including skip-level edges via side channels)
///
/// We consume ascii-dag's LayoutIR and transform it into our own structs
/// that integrate with the TUI's styling and selection logic.
use std::collections::{HashMap, HashSet};

use ascii_dag::DAG;
use workgraph::graph::{Status, Task, WorkGraph};

use super::app::TaskAgentInfo;

// ── Public types ────────────────────────────────────────────────────────

/// A positioned node in the layout, ready for rendering.
#[derive(Debug, Clone)]
pub struct LayoutNode {
    pub task_id: String,
    pub title: String,
    pub status: Status,
    pub critical: bool,
    pub active_agent_count: usize,
    pub active_agent_ids: Vec<String>,
    /// Layer index (0 = top/sources)
    pub layer: usize,
    /// Character column of the left edge of the box
    pub x: usize,
    /// Character row of the top edge of the box
    pub y: usize,
    /// Width of the box in characters (including border)
    pub w: usize,
    /// Height of the box (always 3: top border, content, bottom border)
    pub h: usize,
}

/// An edge between two nodes.
#[derive(Debug, Clone)]
pub struct LayoutEdge {
    /// Segments to draw: list of (x, y) points forming a polyline
    pub segments: Vec<(usize, usize)>,
}

/// A back-edge representing a cycle in the graph.
/// Back-edges point from a descendant back to an ancestor in the DFS tree,
/// creating a cycle. They are rendered with a distinct style (upward arrows,
/// different color) to distinguish them from normal forward edges.
#[derive(Debug, Clone)]
pub struct BackEdge {
    pub from_id: String,
    pub to_id: String,
    /// Segments to draw: list of (x, y) points forming a polyline going upward
    pub segments: Vec<(usize, usize)>,
}

/// A loop edge representing a conditional re-activation edge (loops_to).
/// Distinct from back-edges: loop edges are explicit user-defined re-activation
/// paths, rendered in magenta dashed style with iteration count labels.
#[derive(Debug, Clone)]
pub struct LoopLayoutEdge {
    pub from_id: String,
    pub to_id: String,
    /// Current iteration count on the target task
    pub iteration: u32,
    /// Maximum iterations allowed
    pub max_iterations: u32,
    /// Segments to draw: list of (x, y) points forming a polyline
    pub segments: Vec<(usize, usize)>,
}

/// The complete layout result.
#[derive(Debug)]
pub struct DagLayout {
    pub nodes: Vec<LayoutNode>,
    pub edges: Vec<LayoutEdge>,
    /// Back-edges representing cycles in the graph
    pub back_edges: Vec<BackEdge>,
    /// Loop edges (loops_to) — distinct from back-edges
    pub loop_edges: Vec<LoopLayoutEdge>,
    /// Total width of the layout canvas in characters
    pub width: usize,
    /// Total height of the layout canvas in characters
    pub height: usize,
    /// Mapping from task_id to node index
    pub id_to_idx: HashMap<String, usize>,
    /// Whether the graph contains cycles
    pub has_cycles: bool,
}

// ── Configuration ───────────────────────────────────────────────────────

/// Node box height (top border + content + bottom border)
const NODE_HEIGHT: usize = 3;
/// Minimum node box width (including borders)
const MIN_NODE_WIDTH: usize = 10;
/// Maximum node box width (including borders)
const MAX_NODE_WIDTH: usize = 40;
/// Extra horizontal padding between nodes
const H_PAD: usize = 2;
/// Extra vertical padding between levels (needs room for horizontal routing + arrows)
const V_PAD: usize = 2;
/// Left margin
const LEFT_MARGIN: usize = 1;
/// Top margin
const TOP_MARGIN: usize = 0;
/// Extra right margin for back-edge routing
const BACK_EDGE_MARGIN: usize = 3;

// ── Cycle detection ─────────────────────────────────────────────────────

/// Detect back-edges in the graph using DFS.
///
/// A back-edge is an edge that points from a node to one of its ancestors
/// in the DFS tree, indicating a cycle. This function performs DFS from all
/// roots (nodes with no incoming edges) and identifies any edges that would
/// create cycles.
///
/// Returns a set of (from, to) tuples representing back-edges.
fn detect_back_edges(node_count: usize, edges: &[(usize, usize)]) -> HashSet<(usize, usize)> {
    // Build adjacency list: node -> list of successors
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); node_count];
    let mut in_degree: Vec<usize> = vec![0; node_count];

    for &(from, to) in edges {
        if from < node_count && to < node_count {
            adj[from].push(to);
            in_degree[to] += 1;
        }
    }

    // Find roots (nodes with no incoming edges)
    let roots: Vec<usize> = (0..node_count).filter(|&n| in_degree[n] == 0).collect();

    // DFS state
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Unvisited,
        InStack,  // Currently in the DFS recursion stack (ancestor)
        Finished, // Completely processed
    }

    let mut state = vec![State::Unvisited; node_count];
    let mut back_edges: HashSet<(usize, usize)> = HashSet::new();

    // Iterative DFS to avoid stack overflow on deep graphs
    fn dfs(
        start: usize,
        adj: &[Vec<usize>],
        state: &mut [State],
        back_edges: &mut HashSet<(usize, usize)>,
    ) {
        // Stack stores (node, iterator index into adj[node])
        let mut stack: Vec<(usize, usize)> = vec![(start, 0)];
        state[start] = State::InStack;

        while let Some((node, idx)) = stack.pop() {
            if idx < adj[node].len() {
                let next = adj[node][idx];
                // Push current node back with incremented index
                stack.push((node, idx + 1));

                match state[next] {
                    State::InStack => {
                        // Found a back-edge: edge to an ancestor in the DFS tree
                        back_edges.insert((node, next));
                    }
                    State::Unvisited => {
                        state[next] = State::InStack;
                        stack.push((next, 0));
                    }
                    State::Finished => {
                        // Cross-edge or forward-edge, not a cycle
                    }
                }
            } else {
                // Done with this node
                state[node] = State::Finished;
            }
        }
    }

    // Start DFS from all roots
    for root in roots {
        if state[root] == State::Unvisited {
            dfs(root, &adj, &mut state, &mut back_edges);
        }
    }

    // Also handle disconnected components or cycles with no root
    for node in 0..node_count {
        if state[node] == State::Unvisited {
            dfs(node, &adj, &mut state, &mut back_edges);
        }
    }

    back_edges
}

// ── Layout computation ─────────────────────────────────────────────────

impl DagLayout {
    /// Compute a layered DAG layout from the work graph using ascii-dag.
    pub fn compute(
        graph: &WorkGraph,
        critical_ids: &HashSet<String>,
        agent_map: &HashMap<String, TaskAgentInfo>,
    ) -> Self {
        let tasks: HashMap<String, &Task> = graph.tasks().map(|t| (t.id.clone(), t)).collect();

        if tasks.is_empty() {
            return Self {
                nodes: Vec::new(),
                edges: Vec::new(),
                back_edges: Vec::new(),
                loop_edges: Vec::new(),
                width: 0,
                height: 0,
                id_to_idx: HashMap::new(),
                has_cycles: false,
            };
        }

        // Build a mapping from task ID (String) to a numeric ID for ascii-dag
        // and collect the edges
        // Sort task_ids for deterministic layout ordering (HashMap iteration is non-deterministic)
        let mut task_ids: Vec<String> = tasks.keys().cloned().collect();
        task_ids.sort();
        let id_to_num: HashMap<&str, usize> = task_ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();

        // Build ascii-dag graph
        // Node tuples: (numeric_id, label)
        let nodes_for_dag: Vec<(usize, &str)> = task_ids
            .iter()
            .map(|id| {
                let task = &tasks[id];
                let num = id_to_num[id.as_str()];
                (num, task.title.as_str())
            })
            .collect();

        // Collect all edges: parent -> child (blocker -> blocked)
        let mut all_edges: Vec<(usize, usize)> = Vec::new();
        for task in tasks.values() {
            let child_num = id_to_num[task.id.as_str()];
            for blocker_id in &task.blocked_by {
                if let Some(&parent_num) = id_to_num.get(blocker_id.as_str()) {
                    all_edges.push((parent_num, child_num));
                }
            }
        }

        // Detect back-edges (cycles) using DFS
        let back_edge_set = detect_back_edges(task_ids.len(), &all_edges);

        // Filter out back-edges before passing to ascii-dag
        let edges_for_dag: Vec<(usize, usize)> = all_edges
            .iter()
            .filter(|e| !back_edge_set.contains(e))
            .copied()
            .collect();

        let has_cycles = !back_edge_set.is_empty();

        // Build the DAG and compute layout (now guaranteed acyclic)
        let dag = DAG::from_edges(&nodes_for_dag, &edges_for_dag);
        let ir = dag.compute_layout();

        // Transform ascii-dag's IR into our LayoutNode/LayoutEdge structs
        // ascii-dag uses 1-line-per-node by default; we need to expand to 3-line boxes
        // and compute our own widths based on title length.

        // First pass: compute node widths and map numeric IDs back to task IDs
        let num_to_id: HashMap<usize, &str> =
            id_to_num.iter().map(|(&id, &num)| (num, id)).collect();

        // Collect node info from ascii-dag IR
        let mut node_infos: Vec<(usize, &str, usize, usize, usize)> = Vec::new(); // (num_id, task_id, level, x, width)
        for node in ir.nodes() {
            let task_id = num_to_id.get(&node.id).copied().unwrap_or("");
            node_infos.push((node.id, task_id, node.level, node.x, node.width));
        }

        // Compute our custom widths based on title + status indicator
        let node_widths: HashMap<usize, usize> = node_infos
            .iter()
            .map(|&(num_id, task_id, _, _, _)| {
                let task = tasks.get(task_id);
                let title = task.map(|t| t.title.as_str()).unwrap_or("");
                let indicator =
                    status_indicator_str(&task.map(|t| t.status.clone()).unwrap_or(Status::Open));
                // Box content: " indicator title " + 2 for borders
                let content_width = indicator.len() + 1 + title.len() + 2;
                let w = (content_width + 2).clamp(MIN_NODE_WIDTH, MAX_NODE_WIDTH);
                (num_id, w)
            })
            .collect();

        // Group nodes by level and compute positions
        let level_count = ir.level_count();
        let mut level_nodes: Vec<Vec<(usize, &str)>> = vec![Vec::new(); level_count];
        for &(num_id, task_id, level, _, _) in &node_infos {
            if level < level_count {
                level_nodes[level].push((num_id, task_id));
            }
        }

        // Sort nodes within each level by their ascii-dag x position for consistency
        let node_x_map: HashMap<usize, usize> = node_infos
            .iter()
            .map(|&(num_id, _, _, x, _)| (num_id, x))
            .collect();
        for level in &mut level_nodes {
            level.sort_by_key(|(num_id, _)| node_x_map.get(num_id).copied().unwrap_or(0));
        }

        // Assign x coordinates: pack nodes left-to-right within each level
        let mut node_positions: HashMap<usize, (usize, usize, usize, usize)> = HashMap::new(); // num_id -> (x, y, w, h)
        let mut max_width: usize = 0;
        let mut y = TOP_MARGIN;

        for level in level_nodes.iter() {
            let mut x = LEFT_MARGIN;
            for &(num_id, _) in level.iter() {
                let w = node_widths.get(&num_id).copied().unwrap_or(MIN_NODE_WIDTH);
                node_positions.insert(num_id, (x, y, w, NODE_HEIGHT));
                x += w + H_PAD;
            }
            max_width = max_width.max(x);
            y += NODE_HEIGHT + V_PAD;
        }

        let total_height = if y > V_PAD { y - V_PAD } else { y };

        // Build LayoutNode structs
        let mut layout_nodes: Vec<LayoutNode> = Vec::new();
        for (level_idx, level) in level_nodes.iter().enumerate() {
            for &(num_id, task_id) in level.iter() {
                let (x, y, w, h) = node_positions.get(&num_id).copied().unwrap_or((
                    0,
                    0,
                    MIN_NODE_WIDTH,
                    NODE_HEIGHT,
                ));
                let task = tasks.get(task_id);
                let (agent_count, agent_ids) = agent_map
                    .get(task_id)
                    .map(|info| (info.count, info.agent_ids.clone()))
                    .unwrap_or((0, Vec::new()));

                layout_nodes.push(LayoutNode {
                    task_id: task_id.to_string(),
                    title: task.map(|t| t.title.clone()).unwrap_or_default(),
                    status: task.map(|t| t.status.clone()).unwrap_or(Status::Open),
                    critical: critical_ids.contains(task_id),
                    active_agent_count: agent_count,
                    active_agent_ids: agent_ids,
                    layer: level_idx,
                    x,
                    y,
                    w,
                    h,
                });
            }
        }

        // Build id_to_idx map
        let id_to_idx: HashMap<String, usize> = layout_nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.task_id.clone(), i))
            .collect();

        // Build edges with routing segments
        let mut layout_edges: Vec<LayoutEdge> = Vec::new();
        for edge in ir.edges() {
            let from_task_id = num_to_id.get(&edge.from_id).copied().unwrap_or("");
            let to_task_id = num_to_id.get(&edge.to_id).copied().unwrap_or("");

            if from_task_id.is_empty() || to_task_id.is_empty() {
                continue;
            }

            // Get node positions for routing
            let from_idx = id_to_idx.get(from_task_id);
            let to_idx = id_to_idx.get(to_task_id);

            let (from_node, to_node) = match (from_idx, to_idx) {
                (Some(&fi), Some(&ti)) => (&layout_nodes[fi], &layout_nodes[ti]),
                _ => continue,
            };

            // Route edge: from center-bottom of parent to center-top of child
            let from_x = from_node.x + from_node.w / 2;
            let from_y = from_node.y + from_node.h - 1; // bottom border
            let to_x = to_node.x + to_node.w / 2;
            let to_y = to_node.y; // top border

            let mut segments = Vec::new();
            segments.push((from_x, from_y));

            if from_x == to_x {
                // Straight vertical
                segments.push((to_x, to_y));
            } else {
                // L-shaped or corner routing
                // Place horizontal segment in the gap just below the parent node
                let mid_y = from_y + 1;
                segments.push((from_x, mid_y));
                segments.push((to_x, mid_y));
                segments.push((to_x, to_y));
            }

            layout_edges.push(LayoutEdge { segments });
        }

        // Build back-edges for cycles (will be routed later in reroute_edges)
        // Sort for deterministic ordering (HashSet iteration is non-deterministic)
        let mut back_edge_list: Vec<(usize, usize)> = back_edge_set.into_iter().collect();
        back_edge_list.sort();
        let layout_back_edges: Vec<BackEdge> = back_edge_list
            .iter()
            .filter_map(|&(from_num, to_num)| {
                let from_id = num_to_id.get(&from_num).copied()?;
                let to_id = num_to_id.get(&to_num).copied()?;
                Some(BackEdge {
                    from_id: from_id.to_string(),
                    to_id: to_id.to_string(),
                    segments: Vec::new(), // Will be routed in reroute_edges
                })
            })
            .collect();

        // Build loop edges from tasks' loops_to fields
        let mut layout_loop_edges: Vec<LoopLayoutEdge> = Vec::new();
        for task in tasks.values() {
            for loop_edge in &task.loops_to {
                if tasks.contains_key(&loop_edge.target) {
                    let target_task = &tasks[&loop_edge.target];
                    layout_loop_edges.push(LoopLayoutEdge {
                        from_id: task.id.clone(),
                        to_id: loop_edge.target.clone(),
                        iteration: target_task.loop_iteration,
                        max_iterations: loop_edge.max_iterations,
                        segments: Vec::new(), // Will be routed in reroute_edges
                    });
                }
            }
        }
        // Sort for deterministic ordering
        layout_loop_edges.sort_by(|a, b| (&a.from_id, &a.to_id).cmp(&(&b.from_id, &b.to_id)));

        Self {
            nodes: layout_nodes,
            edges: layout_edges,
            back_edges: layout_back_edges,
            loop_edges: layout_loop_edges,
            width: max_width + LEFT_MARGIN,
            height: total_height,
            id_to_idx,
            has_cycles,
        }
    }

    /// Find a node by task_id
    #[cfg(test)]
    pub fn find_node(&self, task_id: &str) -> Option<&LayoutNode> {
        self.id_to_idx
            .get(task_id)
            .and_then(|&idx| self.nodes.get(idx))
    }
}

// ── Centering helper ────────────────────────────────────────────────────

/// After initial layout, center each layer horizontally relative to the widest layer.
pub fn center_layers(layout: &mut DagLayout) {
    if layout.nodes.is_empty() {
        return;
    }

    // Group nodes by layer
    let max_layer = layout.nodes.iter().map(|n| n.layer).max().unwrap_or(0);
    let mut layer_extents: Vec<(usize, usize)> = vec![(usize::MAX, 0); max_layer.saturating_add(1)];

    for node in &layout.nodes {
        let layer = node.layer;
        layer_extents[layer].0 = layer_extents[layer].0.min(node.x);
        layer_extents[layer].1 = layer_extents[layer].1.max(node.x + node.w);
    }

    // Find widest layer
    let max_width = layer_extents
        .iter()
        .filter(|(min, _)| *min != usize::MAX)
        .map(|(min, max)| max - min)
        .max()
        .unwrap_or(0);

    // Center each layer
    for node in &mut layout.nodes {
        let layer = node.layer;
        let (min_x, max_x) = layer_extents[layer];
        if min_x == usize::MAX {
            continue;
        }
        let layer_width = max_x - min_x;
        let offset = (max_width - layer_width) / 2;
        // Shift relative to the current layer's minimum x
        node.x = node.x - min_x + offset + LEFT_MARGIN;
    }

    // Recompute total width, adding extra margin for back-edge/loop-edge routing
    let has_side_edges = layout.has_cycles || !layout.loop_edges.is_empty();
    let extra_margin = if has_side_edges { BACK_EDGE_MARGIN } else { 0 };
    layout.width = layout
        .nodes
        .iter()
        .map(|n| n.x.saturating_add(n.w))
        .max()
        .unwrap_or(0)
        .saturating_add(LEFT_MARGIN)
        .saturating_add(extra_margin);

    // Rebuild id_to_idx after potential reordering
    layout.id_to_idx = layout
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.task_id.clone(), i))
        .collect();
}

/// Re-route all edges based on current node positions.
///
/// Edge routing strategy (matching ascii-dag's clean output):
/// 1. Vertical down from parent's bottom border center
/// 2. Horizontal routing in the middle of the gap between layers
/// 3. Vertical down to child's top border center
///
/// The arrow is placed on the last vertical segment, just above the child node.
pub fn reroute_edges(layout: &mut DagLayout, graph: &WorkGraph) {
    let tasks: HashMap<String, &Task> = graph.tasks().map(|t| (t.id.clone(), t)).collect();

    // Build a set of back-edge (from, to) pairs to skip when routing normal edges
    let back_edge_set: HashSet<(String, String)> = layout
        .back_edges
        .iter()
        .map(|be| (be.from_id.clone(), be.to_id.clone()))
        .collect();

    let mut new_edges: Vec<LayoutEdge> = Vec::new();

    for node in &layout.nodes {
        let task = match tasks.get(&node.task_id) {
            Some(t) => t,
            None => continue,
        };

        for blocker_id in &task.blocked_by {
            // Skip if this edge is a back-edge (cycle)
            if back_edge_set.contains(&(blocker_id.clone(), node.task_id.clone())) {
                continue;
            }

            let parent_idx = match layout.id_to_idx.get(blocker_id) {
                Some(&i) => i,
                None => continue,
            };

            let parent = &layout.nodes[parent_idx];
            let child = node;

            let from_x = parent.x + parent.w / 2;
            let from_y = parent.y + parent.h - 1; // bottom border
            let to_x = child.x + child.w / 2;
            let to_y = child.y; // top border

            let mut segments = Vec::new();
            segments.push((from_x, from_y));

            if from_x == to_x {
                // Straight vertical - just go down
                segments.push((to_x, to_y));
            } else {
                // Route with horizontal jog at the top of the gap between layers
                // Gap is from (from_y + 1) to (to_y - 1)
                // Put horizontal routing at the top to leave room for arrows below
                let gap_top = from_y + 1;
                segments.push((from_x, gap_top));
                segments.push((to_x, gap_top));
                segments.push((to_x, to_y));
            }

            new_edges.push(LayoutEdge { segments });
        }
    }

    layout.edges = new_edges;

    // Route back-edges (cycles) - these go upward along the right side
    if !layout.back_edges.is_empty() {
        let max_x = layout
            .nodes
            .iter()
            .map(|n| n.x.saturating_add(n.w))
            .max()
            .unwrap_or(0);

        // Route each back-edge along the right margin going upward
        let mut new_back_edges: Vec<BackEdge> = Vec::new();
        for (i, back_edge) in layout.back_edges.iter().enumerate() {
            let from_idx = layout.id_to_idx.get(&back_edge.from_id);
            let to_idx = layout.id_to_idx.get(&back_edge.to_id);

            let (from_node, to_node) = match (from_idx, to_idx) {
                (Some(&fi), Some(&ti)) => (&layout.nodes[fi], &layout.nodes[ti]),
                _ => continue,
            };

            // Back-edge goes from bottom of 'from' node upward to top of 'to' node
            // Route along the right side of the layout
            let route_x = max_x.saturating_add(1).saturating_add(i); // Offset each back-edge slightly for multiple cycles

            // Start from bottom-right of the source node
            let from_x = from_node.x + from_node.w - 1;
            let from_y = from_node.y + from_node.h - 1;

            // End at top-right of the target node
            let to_x = to_node.x + to_node.w - 1;
            let to_y = to_node.y;

            // Route: right from source -> up along margin -> left to target
            let segments = vec![
                (from_x, from_y),  // Start at source
                (route_x, from_y), // Go right to margin
                (route_x, to_y),   // Go up along margin
                (to_x, to_y),      // Go left to target
            ];

            new_back_edges.push(BackEdge {
                from_id: back_edge.from_id.clone(),
                to_id: back_edge.to_id.clone(),
                segments,
            });
        }

        layout.back_edges = new_back_edges;
    }

    // Route loop edges (loops_to) — these go along the right side, offset from back-edges
    if !layout.loop_edges.is_empty() {
        let max_x = layout
            .nodes
            .iter()
            .map(|n| n.x.saturating_add(n.w))
            .max()
            .unwrap_or(0);

        // Offset loop edges after any back-edges to avoid overlap
        let back_edge_count = layout.back_edges.len();

        let mut new_loop_edges: Vec<LoopLayoutEdge> = Vec::new();
        for (i, loop_edge) in layout.loop_edges.iter().enumerate() {
            let from_idx = layout.id_to_idx.get(&loop_edge.from_id);
            let to_idx = layout.id_to_idx.get(&loop_edge.to_id);

            let (from_node, to_node) = match (from_idx, to_idx) {
                (Some(&fi), Some(&ti)) => (&layout.nodes[fi], &layout.nodes[ti]),
                _ => continue,
            };

            // Route along the right side, offset from back-edges
            let route_x = max_x
                .saturating_add(1)
                .saturating_add(back_edge_count)
                .saturating_add(i);

            // Determine routing direction: source → right margin → target
            let from_x = from_node.x + from_node.w - 1;
            let from_y = from_node.y + from_node.h - 1;
            let to_x = to_node.x + to_node.w - 1;
            let to_y = to_node.y;

            let segments = vec![
                (from_x, from_y),
                (route_x, from_y),
                (route_x, to_y),
                (to_x, to_y),
            ];

            new_loop_edges.push(LoopLayoutEdge {
                from_id: loop_edge.from_id.clone(),
                to_id: loop_edge.to_id.clone(),
                iteration: loop_edge.iteration,
                max_iterations: loop_edge.max_iterations,
                segments,
            });
        }

        layout.loop_edges = new_loop_edges;
    }
}

// ── Rendering to a character buffer ─────────────────────────────────────

/// A character cell in the render buffer.
#[derive(Clone)]
pub struct Cell {
    pub ch: char,
    pub style: CellStyle,
}

/// Style for a cell in the render buffer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CellStyle {
    /// Default/empty
    Empty,
    /// Node box border
    Border,
    /// Node content text
    NodeText,
    /// Node content text for active agent
    ActiveAgent,
    /// Node content text for critical path
    Critical,
    /// Node content for done/abandoned
    Dimmed,
    /// Edge line
    Edge,
    /// Edge arrow
    Arrow,
    /// Back-edge line (cycle - goes upward)
    BackEdge,
    /// Back-edge arrow (upward pointing)
    BackEdgeArrow,
    /// Loop edge line (loops_to - magenta dashed)
    LoopEdge,
    /// Loop edge arrow
    LoopEdgeArrow,
    /// Loop edge label (iteration count)
    LoopEdgeLabel,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            style: CellStyle::Empty,
        }
    }
}

/// Render the DAG layout into a 2D character buffer.
///
/// Strategy: use a connectivity grid. For each cell in the inter-layer gap,
/// track which directions (up/down/left/right) have edge connections. Then
/// resolve the correct box-drawing character from the connectivity.
#[allow(clippy::needless_range_loop)] // 2D grid indexing is clearer with range loops
pub fn render_to_buffer(layout: &DagLayout) -> Vec<Vec<Cell>> {
    if layout.width == 0 || layout.height == 0 {
        return Vec::new();
    }

    let width = layout.width + 2; // some extra padding
    let height = layout.height + 1;
    let mut buf: Vec<Vec<Cell>> = vec![vec![Cell::default(); width]; height];

    // Build a connectivity grid: for each cell, track connected directions
    // We'll paint edges into this grid, then resolve characters.
    let mut conn: Vec<Vec<u8>> = vec![vec![0u8; width]; height];
    // Bit flags: UP=1, DOWN=2, LEFT=4, RIGHT=8
    const UP: u8 = 1;
    const DOWN: u8 = 2;
    const LEFT: u8 = 4;
    const RIGHT: u8 = 8;

    // Track which cells are arrow targets
    let mut arrow_cells: HashSet<(usize, usize)> = HashSet::new();

    // Process each edge: paint connectivity into the grid
    for edge in &layout.edges {
        let segs = &edge.segments;
        if segs.len() < 2 {
            continue;
        }

        for i in 0..segs.len() - 1 {
            let (x1, y1) = segs[i];
            let (x2, y2) = segs[i + 1];

            if x1 == x2 {
                // Vertical segment
                let min_y = y1.min(y2);
                let max_y = y1.max(y2);
                for cy in min_y..=max_y {
                    if cy < height && x1 < width {
                        if cy > min_y {
                            conn[cy][x1] |= UP;
                        }
                        if cy < max_y {
                            conn[cy][x1] |= DOWN;
                        }
                    }
                }
            } else if y1 == y2 {
                // Horizontal segment
                let min_x = x1.min(x2);
                let max_x = x1.max(x2);
                for cx in min_x..=max_x {
                    if y1 < height && cx < width {
                        if cx > min_x {
                            conn[y1][cx] |= LEFT;
                        }
                        if cx < max_x {
                            conn[y1][cx] |= RIGHT;
                        }
                    }
                }
            }
        }

        // Mark arrow at the second-to-last point (on the vertical segment leading to target)
        // This places the arrow BELOW the horizontal routing, not overlapping with it.
        // For straight vertical edges, this is one row above the target.
        // For L-shaped edges, this is on the vertical segment going down to the target.
        if segs.len() >= 2 {
            // Get the last segment
            let (tx, ty) = segs[segs.len() - 1];
            // Arrow goes on the cell just above the target node's top border
            if ty > 0 {
                let arrow_y = ty - 1;
                if arrow_y < height && tx < width {
                    arrow_cells.insert((tx, arrow_y));
                    // The arrow cell connects up from above and down to node
                    conn[arrow_y][tx] |= UP;
                }
            }
        }

        // Mark connectivity from the parent's bottom border down to the first edge cell
        if let Some(&(fx, fy)) = segs.first() {
            // fy is the parent's bottom border row; the edge starts just below
            if fy < height && fx < width {
                // The border cell gets DOWN connectivity
                conn[fy][fx] |= DOWN;
                // The cell below gets UP connectivity
                if fy + 1 < height {
                    conn[fy + 1][fx] |= UP;
                }
            }
        }

        // Mark connectivity from the last edge cell down to the target's top border
        if let Some(&(tx, ty)) = segs.last() {
            // ty is the target's top border row
            if ty < height && tx < width {
                conn[ty][tx] |= UP;
                if ty > 0 {
                    conn[ty - 1][tx] |= DOWN;
                }
            }
        }
    }

    // Draw nodes (without edge integration first)
    for node in &layout.nodes {
        draw_node(&mut buf, node, width, height);
    }

    // Now draw edges using the connectivity grid, but only in cells that aren't node content
    for y in 0..height {
        for x in 0..width {
            let c = conn[y][x];
            if c == 0 {
                continue; // no edge connectivity
            }

            let existing = &buf[y][x];

            // Handle arrow cells
            if arrow_cells.contains(&(x, y)) {
                if existing.style == CellStyle::Empty || existing.style == CellStyle::Edge {
                    buf[y][x] = Cell {
                        ch: '▼',
                        style: CellStyle::Arrow,
                    };
                }
                continue;
            }

            // If this cell is on a node border, merge edge connectivity into the border char
            if existing.style == CellStyle::Border {
                let new_ch = merge_border_with_edge(existing.ch, c);
                buf[y][x] = Cell {
                    ch: new_ch,
                    style: CellStyle::Border,
                };
                continue;
            }

            // Skip node content cells
            if existing.style == CellStyle::NodeText
                || existing.style == CellStyle::ActiveAgent
                || existing.style == CellStyle::Critical
                || existing.style == CellStyle::Dimmed
            {
                continue;
            }

            // Empty/edge cell: resolve box-drawing char from connectivity
            let ch = connectivity_to_char(c);
            if ch != ' ' {
                buf[y][x] = Cell {
                    ch,
                    style: CellStyle::Edge,
                };
            }
        }
    }

    // Draw back-edges (cycles) with distinct styling
    // Back-edges are rendered with dashed lines and upward arrows in magenta
    for back_edge in &layout.back_edges {
        let segs = &back_edge.segments;
        if segs.len() < 2 {
            continue;
        }

        // Draw each segment of the back-edge
        for i in 0..segs.len() - 1 {
            let (x1, y1) = segs[i];
            let (x2, y2) = segs[i + 1];

            if x1 == x2 {
                // Vertical segment (going up or down)
                let min_y = y1.min(y2);
                let max_y = y1.max(y2);
                for cy in min_y..=max_y {
                    if cy < height && x1 < width {
                        // Use dashed vertical line for back-edges
                        let ch = '╎';
                        // Don't overwrite node content
                        let existing = &buf[cy][x1];
                        if existing.style == CellStyle::Empty
                            || existing.style == CellStyle::Edge
                            || existing.style == CellStyle::BackEdge
                        {
                            buf[cy][x1] = Cell {
                                ch,
                                style: CellStyle::BackEdge,
                            };
                        }
                    }
                }
            } else if y1 == y2 {
                // Horizontal segment
                let min_x = x1.min(x2);
                let max_x = x1.max(x2);

                for cx in min_x..=max_x {
                    if y1 < height && cx < width {
                        // Use dashed horizontal line for back-edges
                        let existing = &buf[y1][cx];
                        if existing.style == CellStyle::Empty
                            || existing.style == CellStyle::Edge
                            || existing.style == CellStyle::BackEdge
                        {
                            buf[y1][cx] = Cell {
                                ch: '╌',
                                style: CellStyle::BackEdge,
                            };
                        }
                    }
                }
            }
        }

        // Draw corners at segment joints
        for i in 1..segs.len() - 1 {
            let (x, y) = segs[i];
            if y < height && x < width {
                let (px, py) = segs[i - 1];
                let (nx, ny) = segs[i + 1];

                // Determine corner type based on direction changes
                let from_left = px < x;
                let from_right = px > x;
                let from_above = py < y;
                let from_below = py > y;
                let to_left = nx < x;
                let to_right = nx > x;
                let to_above = ny < y;
                let to_below = ny > y;

                let corner_ch = if (from_left || to_left) && (from_above || to_above) {
                    '┘' // coming from left, going up OR coming from above, going left
                } else if (from_right || to_right) && (from_above || to_above) {
                    '└' // coming from right, going up
                } else if (from_left || to_left) && (from_below || to_below) {
                    '┐' // coming from left, going down
                } else if (from_right || to_right) && (from_below || to_below) {
                    '┌' // coming from right, going down
                } else {
                    '+'
                };

                buf[y][x] = Cell {
                    ch: corner_ch,
                    style: CellStyle::BackEdge,
                };
            }
        }

        // Draw upward arrow at the target (last segment end)
        if let Some(&(tx, ty)) = segs.last() {
            // Place arrow just to the right of the target node's top border
            if ty < height && tx < width {
                // The arrow should point to the left (toward the node)
                buf[ty][tx] = Cell {
                    ch: '◀',
                    style: CellStyle::BackEdgeArrow,
                };
            }
        }
    }

    // Draw loop edges (loops_to) with distinct magenta dashed styling
    for loop_edge in &layout.loop_edges {
        let segs = &loop_edge.segments;
        if segs.len() < 2 {
            continue;
        }

        // Draw each segment of the loop edge
        for i in 0..segs.len() - 1 {
            let (x1, y1) = segs[i];
            let (x2, y2) = segs[i + 1];

            if x1 == x2 {
                // Vertical segment
                let min_y = y1.min(y2);
                let max_y = y1.max(y2);
                for cy in min_y..=max_y {
                    if cy < height && x1 < width {
                        let existing = &buf[cy][x1];
                        if existing.style == CellStyle::Empty
                            || existing.style == CellStyle::Edge
                            || existing.style == CellStyle::LoopEdge
                        {
                            buf[cy][x1] = Cell {
                                ch: '┆',
                                style: CellStyle::LoopEdge,
                            };
                        }
                    }
                }
            } else if y1 == y2 {
                // Horizontal segment
                let min_x = x1.min(x2);
                let max_x = x1.max(x2);
                for cx in min_x..=max_x {
                    if y1 < height && cx < width {
                        let existing = &buf[y1][cx];
                        if existing.style == CellStyle::Empty
                            || existing.style == CellStyle::Edge
                            || existing.style == CellStyle::LoopEdge
                        {
                            buf[y1][cx] = Cell {
                                ch: '┄',
                                style: CellStyle::LoopEdge,
                            };
                        }
                    }
                }
            }
        }

        // Draw corners at segment joints
        for i in 1..segs.len() - 1 {
            let (x, y) = segs[i];
            if y < height && x < width {
                let (px, py) = segs[i - 1];
                let (nx, ny) = segs[i + 1];

                let from_left = px < x;
                let from_right = px > x;
                let from_above = py < y;
                let from_below = py > y;
                let to_left = nx < x;
                let to_right = nx > x;
                let to_above = ny < y;
                let to_below = ny > y;

                let corner_ch = if (from_left || to_left) && (from_above || to_above) {
                    '┘'
                } else if (from_right || to_right) && (from_above || to_above) {
                    '└'
                } else if (from_left || to_left) && (from_below || to_below) {
                    '┐'
                } else if (from_right || to_right) && (from_below || to_below) {
                    '┌'
                } else {
                    '+'
                };

                buf[y][x] = Cell {
                    ch: corner_ch,
                    style: CellStyle::LoopEdge,
                };
            }
        }

        // Draw arrow at the target (last segment end)
        if let Some(&(tx, ty)) = segs.last()
            && ty < height
            && tx < width
        {
            buf[ty][tx] = Cell {
                ch: '◀',
                style: CellStyle::LoopEdgeArrow,
            };
        }

        // Draw iteration label on the vertical segment (if there's room)
        // Format: "N/M" where N is current iteration and M is max
        let label = format!("{}/{}", loop_edge.iteration, loop_edge.max_iterations);
        if segs.len() >= 3 {
            // The vertical segment is between segs[1] and segs[2]
            let (vx, vy1) = segs[1];
            let (_, vy2) = segs[2];
            let min_vy = vy1.min(vy2);
            let max_vy = vy1.max(vy2);
            let mid_y = (min_vy + max_vy) / 2;

            // Place label characters starting at mid_y on the vertical segment column
            for (ci, ch) in label.chars().enumerate() {
                let ly = mid_y + ci;
                if ly < height && ly <= max_vy && vx < width {
                    buf[ly][vx] = Cell {
                        ch,
                        style: CellStyle::LoopEdgeLabel,
                    };
                }
            }
        }
    }

    buf
}

/// Merge edge connectivity flags into a node border character.
/// E.g., '─' on bottom border + DOWN connectivity = '┬'
fn merge_border_with_edge(border_ch: char, edge_conn: u8) -> char {
    const UP: u8 = 1;
    const DOWN: u8 = 2;

    match border_ch {
        '─' => {
            if edge_conn & DOWN != 0 && edge_conn & UP != 0 {
                '┼'
            } else if edge_conn & DOWN != 0 {
                '┬'
            } else if edge_conn & UP != 0 {
                '┴'
            } else {
                '─'
            }
        }
        '┌' => {
            if edge_conn & UP != 0 {
                '├'
            } else {
                '┌'
            }
        }
        '┐' => {
            if edge_conn & UP != 0 {
                '┤'
            } else {
                '┐'
            }
        }
        '└' => {
            if edge_conn & DOWN != 0 {
                '├'
            } else {
                '└'
            }
        }
        '┘' => {
            if edge_conn & DOWN != 0 {
                '┤'
            } else {
                '┘'
            }
        }
        '┬' => {
            if edge_conn & UP != 0 {
                '┼'
            } else {
                '┬'
            }
        }
        '┴' => {
            if edge_conn & DOWN != 0 {
                '┼'
            } else {
                '┴'
            }
        }
        other => other,
    }
}

/// Convert connectivity flags to the appropriate box-drawing character.
fn connectivity_to_char(c: u8) -> char {
    const UP: u8 = 1;
    const DOWN: u8 = 2;
    const LEFT: u8 = 4;
    const RIGHT: u8 = 8;

    match c {
        0 => ' ',
        x if x == UP | DOWN | LEFT | RIGHT => '┼',
        x if x == UP | DOWN | LEFT => '┤',
        x if x == UP | DOWN | RIGHT => '├',
        x if x == UP | DOWN => '│',
        x if x == UP | LEFT | RIGHT => '┴',
        x if x == UP | LEFT => '┘',
        x if x == UP | RIGHT => '└',
        x if x == UP => '│',
        x if x == DOWN | LEFT | RIGHT => '┬',
        x if x == DOWN | LEFT => '┐',
        x if x == DOWN | RIGHT => '┌',
        x if x == DOWN => '│',
        x if x == LEFT | RIGHT => '─',
        x if x == LEFT => '─',
        x if x == RIGHT => '─',
        _ => ' ',
    }
}

fn draw_node(buf: &mut [Vec<Cell>], node: &LayoutNode, buf_width: usize, buf_height: usize) {
    let x = node.x;
    let y = node.y;
    let w = node.w;
    let h = node.h;

    if y + h > buf_height || x + w > buf_width {
        return;
    }

    let style = if node.active_agent_count > 0 {
        CellStyle::ActiveAgent
    } else if node.critical {
        CellStyle::Critical
    } else if matches!(node.status, Status::Done | Status::Abandoned) {
        CellStyle::Dimmed
    } else {
        CellStyle::NodeText
    };

    // Top border: ┌───┐
    set_cell(buf, x, y, '┌', CellStyle::Border);
    for cx in (x + 1)..(x + w - 1) {
        set_cell(buf, cx, y, '─', CellStyle::Border);
    }
    set_cell(buf, x + w - 1, y, '┐', CellStyle::Border);

    // Middle row(s): │ content │
    let content_y = y + 1;
    set_cell(buf, x, content_y, '│', CellStyle::Border);
    set_cell(buf, x + w - 1, content_y, '│', CellStyle::Border);

    // Content: status indicator + title, truncated to fit
    let indicator = status_indicator_str(&node.status);
    let max_content = w.saturating_sub(3); // 1 for left padding, 2 for borders
    let full_content = format!("{} {}", indicator, node.title);
    let content: String = if full_content.chars().count() > max_content {
        format!(
            "{}…",
            full_content
                .chars()
                .take(max_content.saturating_sub(1))
                .collect::<String>()
        )
    } else {
        full_content
    };

    // Write content starting at x+1 (after left border), padded with spaces
    for (i, ch) in content.chars().enumerate() {
        let cx = x + 1 + i;
        if cx < x + w - 1 {
            set_cell(buf, cx, content_y, ch, style);
        }
    }
    // Fill remaining with spaces
    for cx in (x + 1 + content.chars().count())..(x + w - 1) {
        set_cell(buf, cx, content_y, ' ', style);
    }

    // Bottom border: └───┘
    let bottom_y = y + h - 1;
    set_cell(buf, x, bottom_y, '└', CellStyle::Border);
    for cx in (x + 1)..(x + w - 1) {
        set_cell(buf, cx, bottom_y, '─', CellStyle::Border);
    }
    set_cell(buf, x + w - 1, bottom_y, '┘', CellStyle::Border);
}

fn set_cell(buf: &mut [Vec<Cell>], x: usize, y: usize, ch: char, style: CellStyle) {
    if y < buf.len() && x < buf.get(y).map_or(0, |row| row.len()) {
        buf[y][x] = Cell { ch, style };
    }
}

fn status_indicator_str(status: &Status) -> &'static str {
    match status {
        Status::Done => "✓",
        Status::InProgress => "~",
        Status::Open => "○",
        Status::Failed => "!",
        Status::Blocked => "B",
        Status::Abandoned => "-",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Node, Status, Task, WorkGraph};

    fn make_task(id: &str, title: &str, blocked_by: Vec<&str>) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
            assigned: None,
            estimate: None,
            blocks: Vec::new(),
            blocked_by: blocked_by.into_iter().map(|s| s.to_string()).collect(),
            requires: Vec::new(),
            tags: Vec::new(),
            skills: Vec::new(),
            inputs: Vec::new(),
            deliverables: Vec::new(),
            artifacts: Vec::new(),
            exec: None,
            not_before: None,
            log: Vec::new(),
            failure_reason: None,
            model: None,
            verify: None,
            agent: None,
            retry_count: 0,
            max_retries: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            loops_to: Vec::new(),
            loop_iteration: 0,
            ready_after: None,
            paused: false,
        }
    }

    fn add_task(graph: &mut WorkGraph, task: Task) {
        graph.add_node(Node::Task(task));
    }

    #[test]
    fn test_dag_layout_simple_chain() {
        let mut graph = WorkGraph::new();
        add_task(&mut graph, make_task("a", "Task A", vec![]));
        add_task(&mut graph, make_task("b", "Task B", vec!["a"]));
        add_task(&mut graph, make_task("c", "Task C", vec!["b"]));

        let critical = HashSet::new();
        let agents = HashMap::new();

        let mut layout = DagLayout::compute(&graph, &critical, &agents);
        center_layers(&mut layout);
        reroute_edges(&mut layout, &graph);

        assert_eq!(layout.nodes.len(), 3);
        assert!(layout.width > 0);
        assert!(layout.height > 0);

        // Verify layers: a=0, b=1, c=2
        let a = layout.find_node("a").unwrap();
        let b = layout.find_node("b").unwrap();
        let c = layout.find_node("c").unwrap();
        assert_eq!(a.layer, 0);
        assert_eq!(b.layer, 1);
        assert_eq!(c.layer, 2);

        // a should be above b, b above c
        assert!(a.y < b.y);
        assert!(b.y < c.y);

        // Verify edges exist
        assert_eq!(layout.edges.len(), 2);

        // Verify render doesn't panic
        let buf = render_to_buffer(&layout);
        assert!(!buf.is_empty());

        // Print the rendered DAG for visual inspection
        let text: String = buf
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|c| c.ch).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("--- Simple chain DAG ---\n{}\n---", text);
    }

    #[test]
    fn test_dag_layout_diamond() {
        let mut graph = WorkGraph::new();
        add_task(&mut graph, make_task("root", "Root", vec![]));
        add_task(&mut graph, make_task("left", "Left", vec!["root"]));
        add_task(&mut graph, make_task("right", "Right", vec!["root"]));
        add_task(
            &mut graph,
            make_task("merge", "Merge", vec!["left", "right"]),
        );

        let critical = HashSet::new();
        let agents = HashMap::new();

        let mut layout = DagLayout::compute(&graph, &critical, &agents);
        center_layers(&mut layout);
        reroute_edges(&mut layout, &graph);

        assert_eq!(layout.nodes.len(), 4);

        // root=layer 0, left/right=layer 1, merge=layer 2
        let root = layout.find_node("root").unwrap();
        let left = layout.find_node("left").unwrap();
        let right = layout.find_node("right").unwrap();
        let merge = layout.find_node("merge").unwrap();
        assert_eq!(root.layer, 0);
        assert_eq!(left.layer, 1);
        assert_eq!(right.layer, 1);
        assert_eq!(merge.layer, 2);

        // left and right should be at the same y
        assert_eq!(left.y, right.y);
        // left and right should have different x positions
        assert_ne!(left.x, right.x);

        // Edges: root->left, root->right, left->merge, right->merge = 4
        assert_eq!(layout.edges.len(), 4);

        let buf = render_to_buffer(&layout);
        assert!(!buf.is_empty());

        let text: String = buf
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|c| c.ch).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("--- Diamond DAG ---\n{}\n---", text);
    }

    #[test]
    fn test_dag_layout_empty_graph() {
        let graph = WorkGraph::new();
        let critical = HashSet::new();
        let agents = HashMap::new();

        let layout = DagLayout::compute(&graph, &critical, &agents);
        assert_eq!(layout.nodes.len(), 0);
        assert_eq!(layout.edges.len(), 0);
        assert_eq!(layout.width, 0);
        assert_eq!(layout.height, 0);

        let buf = render_to_buffer(&layout);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_dag_layout_single_node() {
        let mut graph = WorkGraph::new();
        add_task(&mut graph, make_task("solo", "Solo Task", vec![]));

        let critical = HashSet::new();
        let agents = HashMap::new();

        let mut layout = DagLayout::compute(&graph, &critical, &agents);
        center_layers(&mut layout);
        reroute_edges(&mut layout, &graph);

        assert_eq!(layout.nodes.len(), 1);
        assert_eq!(layout.edges.len(), 0);

        let buf = render_to_buffer(&layout);
        assert!(!buf.is_empty());

        let text: String = buf
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|c| c.ch).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("--- Single node ---\n{}\n---", text);
    }

    #[test]
    fn test_dag_layout_wide_fan_out() {
        let mut graph = WorkGraph::new();
        add_task(&mut graph, make_task("top", "Top Node", vec![]));
        add_task(&mut graph, make_task("c1", "Child 1", vec!["top"]));
        add_task(&mut graph, make_task("c2", "Child 2", vec!["top"]));
        add_task(&mut graph, make_task("c3", "Child 3", vec!["top"]));
        add_task(&mut graph, make_task("c4", "Child 4", vec!["top"]));
        add_task(
            &mut graph,
            make_task("bottom", "Bottom", vec!["c1", "c2", "c3", "c4"]),
        );

        let critical = HashSet::new();
        let agents = HashMap::new();

        let mut layout = DagLayout::compute(&graph, &critical, &agents);
        center_layers(&mut layout);
        reroute_edges(&mut layout, &graph);

        assert_eq!(layout.nodes.len(), 6);
        // 4 from top->c*, 4 from c*->bottom = 8
        assert_eq!(layout.edges.len(), 8);

        let buf = render_to_buffer(&layout);
        let text: String = buf
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|c| c.ch).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("--- Wide fan-out ---\n{}\n---", text);
    }

    #[test]
    fn test_dag_layout_multi_level_dag() {
        // Topology from task description:
        // scaffold -> panel, agent, live-data
        // panel -> graph-exp
        // agent -> graph-exp, (skip to keybinding)
        // live-data -> agent-stream
        // graph-exp -> keybinding
        // agent-stream -> keybinding
        let mut graph = WorkGraph::new();
        add_task(
            &mut graph,
            make_task("scaffold", "scaffold-ratatui", vec![]),
        );
        add_task(&mut graph, make_task("panel", "panel", vec!["scaffold"]));
        add_task(&mut graph, make_task("agent", "agent", vec!["scaffold"]));
        add_task(
            &mut graph,
            make_task("live-data", "live-data", vec!["scaffold"]),
        );
        add_task(
            &mut graph,
            make_task("graph-exp", "graph-exp", vec!["panel", "agent"]),
        );
        add_task(
            &mut graph,
            make_task("agent-stream", "agent-stream", vec!["live-data"]),
        );
        add_task(
            &mut graph,
            make_task(
                "keybinding",
                "tui-keybinding",
                vec!["graph-exp", "agent", "agent-stream"],
            ),
        );

        let critical = HashSet::new();
        let agents = HashMap::new();

        let mut layout = DagLayout::compute(&graph, &critical, &agents);
        center_layers(&mut layout);
        reroute_edges(&mut layout, &graph);

        assert_eq!(layout.nodes.len(), 7);

        let buf = render_to_buffer(&layout);
        let text: String = buf
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|c| c.ch).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("--- Multi-level DAG ---\n{}\n---", text);
    }

    #[test]
    fn test_dag_layout_skip_layer_edge() {
        // A -> B -> D, A -> C -> D, A -> D (skip-layer edge)
        let mut graph = WorkGraph::new();
        add_task(&mut graph, make_task("a", "Task A", vec![]));
        add_task(&mut graph, make_task("b", "Task B", vec!["a"]));
        add_task(&mut graph, make_task("c", "Task C", vec!["a"]));
        add_task(&mut graph, make_task("d", "Task D", vec!["a", "b", "c"]));

        let critical = HashSet::new();
        let agents = HashMap::new();

        let mut layout = DagLayout::compute(&graph, &critical, &agents);
        center_layers(&mut layout);
        reroute_edges(&mut layout, &graph);

        let buf = render_to_buffer(&layout);
        let text: String = buf
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|c| c.ch).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("--- Skip-layer edge ---\n{}\n---", text);
    }

    #[test]
    fn test_dag_layout_simple_cycle() {
        // Test a simple cycle: review -> revise -> review
        let mut graph = WorkGraph::new();
        add_task(&mut graph, make_task("review", "Review", vec!["revise"])); // review depends on revise
        add_task(&mut graph, make_task("revise", "Revise", vec!["review"])); // revise depends on review (CYCLE!)

        let critical = HashSet::new();
        let agents = HashMap::new();

        let mut layout = DagLayout::compute(&graph, &critical, &agents);
        center_layers(&mut layout);
        reroute_edges(&mut layout, &graph);

        // Should detect the cycle
        assert!(
            layout.has_cycles,
            "Should detect cycle in review <-> revise"
        );
        assert_eq!(
            layout.back_edges.len(),
            1,
            "Should have exactly one back-edge"
        );

        // Both nodes should still be laid out
        assert_eq!(layout.nodes.len(), 2);

        // Edges should be split: one normal edge, one back-edge
        assert_eq!(layout.edges.len(), 1, "Should have one normal edge");

        let buf = render_to_buffer(&layout);
        let text: String = buf
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|c| c.ch).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("--- Simple cycle (review <-> revise) ---\n{}\n---", text);
    }

    #[test]
    fn test_dag_layout_chain_with_back_edge() {
        // Test a chain with a back-edge: a -> b -> c -> a
        let mut graph = WorkGraph::new();
        add_task(&mut graph, make_task("a", "Task A", vec!["c"])); // a depends on c (back-edge)
        add_task(&mut graph, make_task("b", "Task B", vec!["a"])); // b depends on a
        add_task(&mut graph, make_task("c", "Task C", vec!["b"])); // c depends on b

        let critical = HashSet::new();
        let agents = HashMap::new();

        let mut layout = DagLayout::compute(&graph, &critical, &agents);
        center_layers(&mut layout);
        reroute_edges(&mut layout, &graph);

        // Should detect the cycle
        assert!(layout.has_cycles, "Should detect cycle in a -> b -> c -> a");
        assert_eq!(
            layout.back_edges.len(),
            1,
            "Should have exactly one back-edge"
        );

        // All nodes should be laid out
        assert_eq!(layout.nodes.len(), 3);

        // Should have 2 normal edges (a->b, b->c) and 1 back-edge (c->a)
        assert_eq!(layout.edges.len(), 2, "Should have two normal edges");

        let buf = render_to_buffer(&layout);
        let text: String = buf
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|c| c.ch).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!(
            "--- Chain with back-edge (a -> b -> c -> a) ---\n{}\n---",
            text
        );
    }

    #[test]
    fn test_dag_layout_multiple_cycles() {
        // Test multiple cycles in the same graph
        // Cycle 1: a <-> b
        // Cycle 2: c <-> d
        let mut graph = WorkGraph::new();
        add_task(&mut graph, make_task("a", "Task A", vec!["b"]));
        add_task(&mut graph, make_task("b", "Task B", vec!["a"]));
        add_task(&mut graph, make_task("c", "Task C", vec!["d"]));
        add_task(&mut graph, make_task("d", "Task D", vec!["c"]));

        let critical = HashSet::new();
        let agents = HashMap::new();

        let mut layout = DagLayout::compute(&graph, &critical, &agents);
        center_layers(&mut layout);
        reroute_edges(&mut layout, &graph);

        // Should detect both cycles
        assert!(layout.has_cycles, "Should detect cycles");
        assert_eq!(layout.back_edges.len(), 2, "Should have two back-edges");

        // All nodes should be laid out
        assert_eq!(layout.nodes.len(), 4);

        let buf = render_to_buffer(&layout);
        let text: String = buf
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|c| c.ch).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        eprintln!("--- Multiple cycles ---\n{}\n---", text);
    }

    #[test]
    fn test_dag_layout_acyclic_graph_no_back_edges() {
        // Verify that acyclic graphs have no back-edges
        let mut graph = WorkGraph::new();
        add_task(&mut graph, make_task("root", "Root", vec![]));
        add_task(&mut graph, make_task("left", "Left", vec!["root"]));
        add_task(&mut graph, make_task("right", "Right", vec!["root"]));
        add_task(
            &mut graph,
            make_task("merge", "Merge", vec!["left", "right"]),
        );

        let critical = HashSet::new();
        let agents = HashMap::new();

        let mut layout = DagLayout::compute(&graph, &critical, &agents);
        center_layers(&mut layout);
        reroute_edges(&mut layout, &graph);

        // Should NOT detect any cycles
        assert!(!layout.has_cycles, "Acyclic graph should have no cycles");
        assert_eq!(layout.back_edges.len(), 0, "Should have no back-edges");

        // All 4 edges should be normal edges
        assert_eq!(layout.edges.len(), 4, "Should have four normal edges");
    }

    #[test]
    fn test_detect_back_edges_function() {
        // Direct test of the detect_back_edges function

        // Simple cycle: 0 -> 1 -> 0
        let edges = vec![(0, 1), (1, 0)];
        let back_edges = detect_back_edges(2, &edges);
        assert_eq!(
            back_edges.len(),
            1,
            "Should detect one back-edge in simple cycle"
        );

        // Chain with back-edge: 0 -> 1 -> 2 -> 0
        let edges = vec![(0, 1), (1, 2), (2, 0)];
        let back_edges = detect_back_edges(3, &edges);
        assert_eq!(
            back_edges.len(),
            1,
            "Should detect one back-edge in chain cycle"
        );

        // Acyclic diamond: 0 -> 1, 0 -> 2, 1 -> 3, 2 -> 3
        let edges = vec![(0, 1), (0, 2), (1, 3), (2, 3)];
        let back_edges = detect_back_edges(4, &edges);
        assert_eq!(
            back_edges.len(),
            0,
            "Should detect no back-edges in acyclic graph"
        );

        // Self-loop: 0 -> 0
        let edges = vec![(0, 0)];
        let back_edges = detect_back_edges(1, &edges);
        assert_eq!(back_edges.len(), 1, "Should detect self-loop as back-edge");
    }

    #[test]
    fn test_set_cell_empty_buffer_no_panic() {
        let mut buf: Vec<Vec<Cell>> = vec![];
        // Should not panic on empty buffer
        set_cell(&mut buf, 0, 0, 'x', CellStyle::Empty);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_render_unicode_title_no_gap() {
        // Unicode titles should render correctly without gaps
        let mut graph = WorkGraph::new();
        let mut task = make_task("uni", "Task ✓ 完了", vec![]);
        task.status = Status::Done;
        add_task(&mut graph, task);

        let critical = HashSet::new();
        let agents = HashMap::new();

        let mut layout = DagLayout::compute(&graph, &critical, &agents);
        center_layers(&mut layout);
        reroute_edges(&mut layout, &graph);

        // Should render without panic
        let buf = render_to_buffer(&layout);
        assert!(!buf.is_empty());

        // Verify no default '\0' chars remain in the content area
        // (which would indicate gap from byte vs char mismatch)
        let text: String = buf
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|c| c.ch).collect();
                line.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.contains('\0'),
            "Buffer should not contain null chars from byte/char mismatch"
        );
    }

    #[test]
    fn test_dag_layout_deterministic_ordering() {
        // Test that layout is deterministic across multiple runs
        // This verifies the fix for HashMap/HashSet non-deterministic iteration
        let mut graph = WorkGraph::new();
        add_task(&mut graph, make_task("z", "Task Z", vec![]));
        add_task(&mut graph, make_task("a", "Task A", vec!["z"]));
        add_task(&mut graph, make_task("m", "Task M", vec!["z"]));
        add_task(&mut graph, make_task("b", "Task B", vec!["a", "m"]));

        let critical = HashSet::new();
        let agents = HashMap::new();

        // Compute layout multiple times and verify consistency
        let mut first_node_order: Option<Vec<String>> = None;
        let mut first_node_positions: Option<Vec<(usize, usize)>> = None;

        for iteration in 0..10 {
            let mut layout = DagLayout::compute(&graph, &critical, &agents);
            center_layers(&mut layout);
            reroute_edges(&mut layout, &graph);

            let node_order: Vec<String> = layout.nodes.iter().map(|n| n.task_id.clone()).collect();
            let node_positions: Vec<(usize, usize)> =
                layout.nodes.iter().map(|n| (n.x, n.y)).collect();

            if let Some(ref first) = first_node_order {
                assert_eq!(
                    &node_order, first,
                    "Node order changed on iteration {}",
                    iteration
                );
            } else {
                first_node_order = Some(node_order);
            }

            if let Some(ref first) = first_node_positions {
                assert_eq!(
                    &node_positions, first,
                    "Node positions changed on iteration {}",
                    iteration
                );
            } else {
                first_node_positions = Some(node_positions);
            }
        }
    }
}
