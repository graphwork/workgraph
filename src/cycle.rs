//! Cycle detection and loop analysis for directed graphs.
//!
//! This module provides standalone cycle detection algorithms that can be used
//! independently of workgraph's graph structures. The primary algorithms are:
//!
//! 1. **Tarjan's SCC** — finds all strongly connected components in O(V+E) time.
//!    Uses an iterative implementation to avoid stack overflow on large graphs.
//!    Reference: Tarjan, "Depth-First Search and Linear Graph Algorithms," SIAM 1972.
//!
//! 2. **Havlak's Loop Nesting Forest** — identifies loop headers and nesting
//!    structure for both reducible and irreducible loops.
//!    Reference: Havlak, "Nesting of Reducible and Irreducible Loops," TOPLAS 1997.
//!    Complexity fix: Ramalingam, "Identifying Loops in Almost Linear Time," TOPLAS 1999.
//!
//! 3. **Incremental Cycle Detection** — detects whether adding a single edge
//!    creates a cycle, and if so identifies the cycle members, without
//!    recomputing the entire graph. O(affected nodes) per edge insertion.
//!
//! 4. **Cycle Metadata Extraction** — given detected cycles, extracts member
//!    tasks, header task, nesting depth, and iteration state for workgraph
//!    integration.
//!
//! # Graph Representation
//!
//! All algorithms operate on a directed graph represented as an adjacency list:
//! `HashMap<NodeId, Vec<NodeId>>` where each key maps to its successors.
//! This is independent of workgraph's internal data structures so the module
//! can be tested in isolation.

use std::collections::{HashMap, HashSet, VecDeque};

/// A node identifier. Using usize for efficiency; callers map their own IDs.
pub type NodeId = usize;

// ─────────────────────────────────────────────────────────────────────────────
// 1. Tarjan's SCC Algorithm (Iterative)
// ─────────────────────────────────────────────────────────────────────────────

/// A strongly connected component: a maximal set of nodes where every node
/// is reachable from every other node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scc {
    /// The node IDs in this SCC, in no particular order.
    pub members: Vec<NodeId>,
}

/// Finds all strongly connected components in a directed graph using
/// Tarjan's algorithm.
///
/// Uses an **iterative** DFS to avoid stack overflow on large graphs.
/// Based on Tarjan (1972) with the iterative transformation from
/// Pearce (2016).
///
/// # Arguments
/// * `num_nodes` — total number of nodes (IDs are 0..num_nodes)
/// * `adj` — adjacency list: adj[u] = list of successors of u
///
/// # Returns
/// All SCCs in reverse topological order of the condensation DAG.
/// Each SCC with >1 member contains at least one cycle.
/// Single-node SCCs may or may not have self-loops.
///
/// # Complexity
/// * Time: O(V + E)
/// * Space: O(V)
///
/// # Example
/// ```
/// use workgraph::cycle::tarjan_scc;
///
/// // Graph: 0 → 1 → 2 → 0 (a 3-node cycle)
/// let adj = vec![vec![1], vec![2], vec![0]];
/// let sccs = tarjan_scc(3, &adj);
/// assert_eq!(sccs.len(), 1);
/// assert_eq!(sccs[0].members.len(), 3);
/// ```
pub fn tarjan_scc(num_nodes: usize, adj: &[Vec<NodeId>]) -> Vec<Scc> {
    // State per node
    const UNDEFINED: i32 = -1;
    let mut index = vec![UNDEFINED; num_nodes]; // DFS index
    let mut lowlink = vec![0i32; num_nodes]; // lowlink value
    let mut on_stack = vec![false; num_nodes]; // whether node is on the SCC stack

    let mut stack: Vec<NodeId> = Vec::new(); // SCC stack
    let mut current_index: i32 = 0;
    let mut result: Vec<Scc> = Vec::new();

    // Iterative DFS using an explicit call stack.
    // Each frame tracks: (node, neighbor_iterator_position)
    struct Frame {
        node: NodeId,
        next_neighbor: usize, // index into adj[node]
    }

    for start in 0..num_nodes {
        if index[start] != UNDEFINED {
            continue;
        }

        let mut call_stack: Vec<Frame> = vec![Frame {
            node: start,
            next_neighbor: 0,
        }];

        // Initialize the start node
        index[start] = current_index;
        lowlink[start] = current_index;
        current_index += 1;
        stack.push(start);
        on_stack[start] = true;

        while let Some(frame) = call_stack.last_mut() {
            let v = frame.node;

            if frame.next_neighbor < adj[v].len() {
                let w = adj[v][frame.next_neighbor];
                frame.next_neighbor += 1;

                if index[w] == UNDEFINED {
                    // Tree edge: "recurse" into w
                    index[w] = current_index;
                    lowlink[w] = current_index;
                    current_index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    call_stack.push(Frame {
                        node: w,
                        next_neighbor: 0,
                    });
                } else if on_stack[w] {
                    // Back edge: update lowlink
                    lowlink[v] = lowlink[v].min(index[w]);
                }
            } else {
                // All neighbors processed — check if v is an SCC root
                if lowlink[v] == index[v] {
                    let mut scc_members = Vec::new();
                    loop {
                        let w = stack.pop().unwrap();
                        on_stack[w] = false;
                        scc_members.push(w);
                        if w == v {
                            break;
                        }
                    }
                    result.push(Scc {
                        members: scc_members,
                    });
                }

                // "Return" from recursion: propagate lowlink to parent
                let finished = call_stack.pop().unwrap();
                if let Some(parent) = call_stack.last_mut() {
                    let p = parent.node;
                    lowlink[p] = lowlink[p].min(lowlink[finished.node]);
                }
            }
        }
    }

    result
}

/// Returns only non-trivial SCCs (size > 1), which represent actual cycles.
///
/// Single-node SCCs without self-loops are filtered out since they don't
/// represent cycles. Self-loops (node with an edge to itself) are included
/// if `include_self_loops` is true.
///
/// # Complexity
/// * Time: O(V + E)
/// * Space: O(V)
pub fn find_cycles(
    num_nodes: usize,
    adj: &[Vec<NodeId>],
    include_self_loops: bool,
) -> Vec<Scc> {
    let sccs = tarjan_scc(num_nodes, adj);
    sccs.into_iter()
        .filter(|scc| {
            if scc.members.len() > 1 {
                return true;
            }
            if include_self_loops && scc.members.len() == 1 {
                let n = scc.members[0];
                return adj[n].contains(&n);
            }
            false
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// 2. Havlak Loop Nesting Forest
// ─────────────────────────────────────────────────────────────────────────────

/// A node in the loop nesting forest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopNode {
    /// The loop header node ID.
    pub header: NodeId,
    /// All nodes in this loop's body (including the header).
    pub body: Vec<NodeId>,
    /// Back edges that form this loop: (tail, head) where head == header.
    pub back_edges: Vec<(NodeId, NodeId)>,
    /// Whether this is a reducible loop (single entry point).
    pub reducible: bool,
    /// Nesting depth (0 = outermost loop).
    pub depth: usize,
    /// Parent loop header (None if this is a top-level loop).
    pub parent: Option<NodeId>,
    /// Direct child loop headers.
    pub children: Vec<NodeId>,
}

/// The loop nesting forest for a directed graph.
///
/// Built using a simplified version of Havlak's algorithm (1997) with
/// Ramalingam's complexity fix (1999). Identifies loop headers, bodies,
/// nesting relationships, and back edges.
///
/// Handles both reducible loops (single entry point, like `while` loops)
/// and irreducible loops (multiple entry points, like mutual gotos).
#[derive(Debug, Clone)]
pub struct LoopNestingForest {
    /// All loops, keyed by their header node.
    pub loops: HashMap<NodeId, LoopNode>,
    /// Which loop header each node belongs to (innermost).
    /// Nodes not in any loop are absent.
    pub node_to_loop: HashMap<NodeId, NodeId>,
    /// Top-level loop headers (not nested inside any other loop).
    pub roots: Vec<NodeId>,
}

/// Builds a loop nesting forest for the directed graph.
///
/// Uses DFS from `entry` to discover loops. Nodes unreachable from `entry`
/// are not analyzed.
///
/// # Algorithm
///
/// 1. Perform iterative DFS to compute DFS numbering and classify edges.
/// 2. Identify back edges: edges (u, v) where v is an ancestor of u in
///    the DFS tree (i.e., v has a smaller DFS number and u is in v's subtree).
/// 3. For each back-edge target (potential loop header), find the loop body
///    by backward BFS from the back-edge source, stopping at the header.
/// 4. Detect irreducible loops: loops where some body node has a predecessor
///    outside the loop that is not the header.
/// 5. Build the nesting hierarchy based on containment.
///
/// # Complexity
/// * Time: O(V + E) amortized (with Ramalingam's fix for irreducible loops)
/// * Space: O(V + E)
///
/// # References
/// - Havlak, "Nesting of Reducible and Irreducible Loops," TOPLAS 1997
/// - Ramalingam, "Identifying Loops in Almost Linear Time," TOPLAS 1999
///
/// # Example
/// ```
/// use workgraph::cycle::build_loop_nesting_forest;
///
/// // Graph: 0 → 1 → 2 → 1 (loop: 1→2→1, entry via 0)
/// let adj = vec![vec![1], vec![2], vec![1]];
/// let forest = build_loop_nesting_forest(3, &adj, 0);
/// assert_eq!(forest.loops.len(), 1);
/// assert!(forest.loops.contains_key(&1)); // header is node 1
/// ```
pub fn build_loop_nesting_forest(
    num_nodes: usize,
    adj: &[Vec<NodeId>],
    entry: NodeId,
) -> LoopNestingForest {
    // Step 1: Iterative DFS to compute pre-order numbering and identify back edges
    let mut dfs_num = vec![-1i32; num_nodes]; // DFS pre-order number
    let mut dfs_end = vec![-1i32; num_nodes]; // DFS post-order number (for ancestor check)
    let mut counter = 0i32;
    let mut post_counter = 0i32;
    let mut back_edges: Vec<(NodeId, NodeId)> = Vec::new();

    // Iterative DFS
    {
        let mut stack: Vec<(NodeId, usize, bool)> = Vec::new(); // (node, neighbor_idx, is_return)
        dfs_num[entry] = counter;
        counter += 1;
        stack.push((entry, 0, false));

        while let Some((v, ni, is_return)) = stack.last_mut() {
            let v = *v;
            if *is_return {
                // Returning from a child — set post-order
                dfs_end[v] = post_counter;
                post_counter += 1;
                stack.pop();
                continue;
            }

            if *ni < adj[v].len() {
                let w = adj[v][*ni];
                *ni += 1;

                if dfs_num[w] == -1 {
                    // Tree edge
                    dfs_num[w] = counter;
                    counter += 1;
                    stack.push((w, 0, false));
                } else if dfs_end[w] == -1 {
                    // w is an ancestor (still on the DFS stack, no post-order yet)
                    back_edges.push((v, w));
                }
                // else: cross edge or forward edge — ignore
            } else {
                // All neighbors processed
                *is_return = true;
            }
        }
    }

    // Step 2: Group back edges by header (target of back edge)
    let mut header_back_edges: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for &(tail, head) in &back_edges {
        header_back_edges.entry(head).or_default().push(tail);
    }

    // Step 3: For each header, find the loop body via backward BFS
    // Process headers in reverse DFS order (inner loops first)
    let mut headers_by_dfs: Vec<NodeId> = header_back_edges.keys().copied().collect();
    headers_by_dfs.sort_by(|a, b| dfs_num[*b].cmp(&dfs_num[*a])); // reverse DFS order

    // Build reverse adjacency list for backward BFS
    let mut rev_adj: Vec<Vec<NodeId>> = vec![Vec::new(); num_nodes];
    for (u, succs) in adj.iter().enumerate() {
        for &v in succs {
            if dfs_num[u] != -1 && dfs_num[v] != -1 {
                rev_adj[v].push(u);
            }
        }
    }

    let mut node_to_loop: HashMap<NodeId, NodeId> = HashMap::new();
    let mut loops: HashMap<NodeId, LoopNode> = HashMap::new();

    for &header in &headers_by_dfs {
        let back_edge_tails = &header_back_edges[&header];
        let mut body: HashSet<NodeId> = HashSet::new();
        body.insert(header);

        // Backward BFS from each back-edge tail to find the loop body.
        // Only include nodes with dfs_num >= header's dfs_num — ancestors
        // of the header are NOT part of the loop body.
        let header_dfs = dfs_num[header];
        let mut queue: VecDeque<NodeId> = VecDeque::new();
        for &tail in back_edge_tails {
            if tail != header && body.insert(tail) {
                queue.push_back(tail);
            }
        }

        while let Some(node) = queue.pop_front() {
            for &pred in &rev_adj[node] {
                if dfs_num[pred] >= header_dfs && body.insert(pred)
                    && pred != header {
                        queue.push_back(pred);
                    }
            }
        }

        // Step 4: Check for irreducibility
        // A loop is irreducible if some body node (other than the header)
        // has a predecessor from outside the loop that can reach it without
        // going through the header.
        let mut reducible = true;
        for &node in &body {
            if node == header {
                continue;
            }
            for &pred in &rev_adj[node] {
                if !body.contains(&pred) {
                    // External predecessor to a non-header body node
                    // This means there's another entry point — irreducible
                    reducible = false;
                    break;
                }
            }
            if !reducible {
                break;
            }
        }

        let back_edge_list: Vec<(NodeId, NodeId)> =
            back_edge_tails.iter().map(|&t| (t, header)).collect();

        let body_vec: Vec<NodeId> = {
            let mut v: Vec<NodeId> = body.iter().copied().collect();
            v.sort_unstable();
            v
        };

        // Assign nodes to this loop (innermost wins — processed inner first)
        for &node in &body_vec {
            node_to_loop.entry(node).or_insert(header);
        }

        loops.insert(
            header,
            LoopNode {
                header,
                body: body_vec,
                back_edges: back_edge_list,
                reducible,
                depth: 0, // computed below
                parent: None,
                children: Vec::new(),
            },
        );
    }

    // Step 5: Build nesting hierarchy
    // A loop L1 (header h1) is nested inside L2 (header h2) if h1 is in L2's body
    // and h1 != h2. The innermost enclosing loop is the parent.
    let all_headers: Vec<NodeId> = loops.keys().copied().collect();
    for &h1 in &all_headers {
        let mut best_parent: Option<NodeId> = None;
        let mut best_size = usize::MAX;
        for &h2 in &all_headers {
            if h1 == h2 {
                continue;
            }
            let l2 = &loops[&h2];
            if l2.body.contains(&h1) && l2.body.len() < best_size {
                best_parent = Some(h2);
                best_size = l2.body.len();
            }
        }
        if let Some(parent) = best_parent {
            loops.get_mut(&h1).unwrap().parent = Some(parent);
            loops.get_mut(&parent).unwrap().children.push(h1);
        }
    }

    // Compute depths
    fn compute_depth(header: NodeId, loops: &mut HashMap<NodeId, LoopNode>) {
        let children: Vec<NodeId> = loops[&header].children.clone();
        for child in children {
            loops.get_mut(&child).unwrap().depth = loops[&header].depth + 1;
            compute_depth(child, loops);
        }
    }

    let roots: Vec<NodeId> = loops
        .values()
        .filter(|l| l.parent.is_none())
        .map(|l| l.header)
        .collect();

    for &root in &roots {
        compute_depth(root, &mut loops);
    }

    LoopNestingForest {
        loops,
        node_to_loop,
        roots,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 3. Incremental Cycle Detection
// ─────────────────────────────────────────────────────────────────────────────

/// Result of checking whether adding an edge creates a cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeAddResult {
    /// No cycle created — the edge is safe to add.
    NoCycle,
    /// Adding this edge creates a cycle. Contains the cycle members
    /// in traversal order (from the target of the new edge back to the source).
    CreatesCycle {
        /// Nodes in the cycle, starting from `to` and ending at `from`.
        cycle_members: Vec<NodeId>,
    },
}

/// Checks whether adding a directed edge `from → to` would create a cycle
/// in the given acyclic graph.
///
/// If a cycle would be created, returns the cycle members. This works by
/// checking whether `to` can already reach `from` (via BFS/DFS on the
/// existing graph). If so, adding `from → to` would close the cycle.
///
/// This is much cheaper than recomputing all SCCs when only one edge changes.
///
/// # Arguments
/// * `num_nodes` — total number of nodes
/// * `adj` — current adjacency list (assumed acyclic)
/// * `from` — source of the new edge
/// * `to` — target of the new edge
///
/// # Returns
/// `EdgeAddResult::NoCycle` if the edge is safe, or
/// `EdgeAddResult::CreatesCycle` with the cycle members if it would create a cycle.
///
/// # Complexity
/// * Time: O(reachable nodes from `to`) — NOT O(V+E) for the full graph
/// * Space: O(reachable nodes from `to`)
///
/// # Example
/// ```
/// use workgraph::cycle::{check_edge_addition, EdgeAddResult};
///
/// // Graph: 0 → 1 → 2 (no cycles)
/// let adj = vec![vec![1], vec![2], vec![]];
/// // Adding 2 → 0 would create cycle 0 → 1 → 2 → 0
/// let result = check_edge_addition(3, &adj, 2, 0);
/// match result {
///     EdgeAddResult::CreatesCycle { cycle_members } => {
///         assert_eq!(cycle_members.len(), 3);
///     }
///     _ => panic!("expected cycle"),
/// }
/// ```
pub fn check_edge_addition(
    num_nodes: usize,
    adj: &[Vec<NodeId>],
    from: NodeId,
    to: NodeId,
) -> EdgeAddResult {
    // Self-loop
    if from == to {
        return EdgeAddResult::CreatesCycle {
            cycle_members: vec![from],
        };
    }

    // BFS from `to` to see if we can reach `from`
    let mut visited = vec![false; num_nodes];
    let mut parent: Vec<Option<NodeId>> = vec![None; num_nodes];
    let mut queue = VecDeque::new();

    visited[to] = true;
    queue.push_back(to);

    let mut found = false;
    while let Some(node) = queue.pop_front() {
        if node == from {
            found = true;
            break;
        }
        for &next in &adj[node] {
            if !visited[next] {
                visited[next] = true;
                parent[next] = Some(node);
                queue.push_back(next);
            }
        }
    }

    if !found {
        return EdgeAddResult::NoCycle;
    }

    // Reconstruct the path from `to` to `from`, which together with the
    // proposed edge `from → to` forms the cycle.
    let mut path = Vec::new();
    let mut current = from;
    while current != to {
        path.push(current);
        current = parent[current].unwrap();
    }
    path.push(to);
    path.reverse(); // now: to → ... → from

    EdgeAddResult::CreatesCycle {
        cycle_members: path,
    }
}

/// Incrementally maintains a topological order and detects cycles on edge
/// additions. More efficient than `check_edge_addition` when many edges
/// are added sequentially, because it maintains state across insertions.
///
/// Based on the approach from Bender, Fineman & Gilbert (2016):
/// "A New Approach to Incremental Cycle Detection."
///
/// # Complexity
/// * Time: O(affected nodes) per edge insertion, amortized
/// * Space: O(V)
pub struct IncrementalCycleDetector {
    num_nodes: usize,
    adj: Vec<Vec<NodeId>>,
    /// Topological order value for each node. Lower = earlier in the order.
    topo_order: Vec<i64>,
}

impl IncrementalCycleDetector {
    /// Creates a new incremental detector for `num_nodes` nodes with no edges.
    ///
    /// All nodes start with a default topological order (their index).
    pub fn new(num_nodes: usize) -> Self {
        Self {
            num_nodes,
            adj: vec![Vec::new(); num_nodes],
            topo_order: (0..num_nodes as i64).collect(),
        }
    }

    /// Creates a detector from an existing acyclic adjacency list.
    ///
    /// Computes an initial topological order via Kahn's algorithm.
    /// Panics if the graph already contains cycles.
    pub fn from_acyclic(num_nodes: usize, adj: Vec<Vec<NodeId>>) -> Self {
        // Compute topological order via Kahn's algorithm
        let mut in_degree = vec![0usize; num_nodes];
        for succs in &adj {
            for &v in succs {
                in_degree[v] += 1;
            }
        }

        let mut queue: VecDeque<NodeId> = VecDeque::new();
        for (i, &deg) in in_degree.iter().enumerate() {
            if deg == 0 {
                queue.push_back(i);
            }
        }

        let mut topo_order = vec![0i64; num_nodes];
        let mut order = 0i64;
        let mut count = 0usize;
        while let Some(node) = queue.pop_front() {
            topo_order[node] = order;
            order += 1;
            count += 1;
            for &next in &adj[node] {
                in_degree[next] -= 1;
                if in_degree[next] == 0 {
                    queue.push_back(next);
                }
            }
        }

        assert_eq!(count, num_nodes, "Graph contains cycles — cannot build IncrementalCycleDetector from cyclic graph");

        Self {
            num_nodes,
            adj,
            topo_order,
        }
    }

    /// Attempts to add edge `from → to`.
    ///
    /// Returns `Ok(())` if the edge was added successfully (no cycle),
    /// or `Err(cycle_members)` if adding the edge would create a cycle.
    ///
    /// If a cycle would be created, the edge is NOT added.
    ///
    /// # Complexity
    /// * Time: O(affected nodes) — only visits nodes between `to` and `from`
    ///   in the topological order.
    pub fn add_edge(&mut self, from: NodeId, to: NodeId) -> Result<(), Vec<NodeId>> {
        if from == to {
            return Err(vec![from]);
        }

        // If `from` is already before `to` in topological order, no cycle possible
        if self.topo_order[from] < self.topo_order[to] {
            self.adj[from].push(to);
            return Ok(());
        }

        // Potential cycle: check if `to` can reach `from` via existing edges.
        // Only need to check nodes with topo_order in [topo_order[to], topo_order[from]].
        let hi = self.topo_order[from];

        let mut visited = HashSet::new();
        let mut parent: HashMap<NodeId, NodeId> = HashMap::new();
        let mut queue = VecDeque::new();
        visited.insert(to);
        queue.push_back(to);

        let mut found_cycle = false;
        while let Some(node) = queue.pop_front() {
            if node == from {
                found_cycle = true;
                break;
            }
            for &next in &self.adj[node] {
                if !visited.contains(&next) && self.topo_order[next] <= hi {
                    visited.insert(next);
                    parent.insert(next, node);
                    queue.push_back(next);
                }
            }
        }

        if found_cycle {
            // Reconstruct cycle path
            let mut path = Vec::new();
            let mut cur = from;
            while cur != to {
                path.push(cur);
                cur = *parent.get(&cur).unwrap();
            }
            path.push(to);
            path.reverse();
            return Err(path);
        }

        // No cycle — add the edge and reorder affected nodes.
        // Nodes reachable from `to` with topo_order <= hi need to be moved after `from`.
        self.adj[from].push(to);

        // Collect affected nodes (those visited during the search)
        let mut affected: Vec<NodeId> = visited.into_iter().collect();
        affected.sort_by_key(|&n| self.topo_order[n]);

        // Collect the topo_order values of affected nodes (sorted)
        let mut slots: Vec<i64> = affected.iter().map(|&n| self.topo_order[n]).collect();
        slots.sort_unstable();

        // Topological sort the affected subgraph
        let affected_set: HashSet<NodeId> = affected.iter().copied().collect();
        let mut local_in_degree: HashMap<NodeId, usize> = HashMap::new();
        for &n in &affected {
            local_in_degree.insert(n, 0);
        }
        for &n in &affected {
            for &next in &self.adj[n] {
                if affected_set.contains(&next) {
                    *local_in_degree.get_mut(&next).unwrap() += 1;
                }
            }
        }

        let mut q: VecDeque<NodeId> = VecDeque::new();
        for (&n, &deg) in &local_in_degree {
            if deg == 0 {
                q.push_back(n);
            }
        }

        let mut sorted = Vec::new();
        while let Some(n) = q.pop_front() {
            sorted.push(n);
            for &next in &self.adj[n] {
                if let Some(deg) = local_in_degree.get_mut(&next) {
                    *deg -= 1;
                    if *deg == 0 {
                        q.push_back(next);
                    }
                }
            }
        }

        // Reassign topo_order values to the sorted affected nodes
        for (i, &node) in sorted.iter().enumerate() {
            self.topo_order[node] = slots[i];
        }

        Ok(())
    }

    /// Returns a reference to the current adjacency list.
    pub fn adjacency(&self) -> &[Vec<NodeId>] {
        &self.adj
    }

    /// Returns the number of nodes.
    pub fn num_nodes(&self) -> usize {
        self.num_nodes
    }

    /// Returns the current topological order value for a node.
    pub fn topo_order_of(&self, node: NodeId) -> i64 {
        self.topo_order[node]
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 4. Cycle Metadata Extraction
// ─────────────────────────────────────────────────────────────────────────────

/// Metadata about a detected cycle, suitable for workgraph integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleMetadata {
    /// All node IDs in this cycle's SCC.
    pub members: Vec<NodeId>,
    /// The cycle header (entry point). Determined by:
    /// 1. If exactly one node has predecessors from outside the SCC → that node.
    /// 2. If no external predecessors → the smallest node ID.
    /// 3. If multiple external predecessors → the smallest such node (irreducible).
    pub header: NodeId,
    /// Whether the cycle is reducible (single entry point).
    pub reducible: bool,
    /// Back edges: edges within the SCC that point to the header.
    pub back_edges: Vec<(NodeId, NodeId)>,
    /// Nesting depth (0 = not nested, 1 = inside one outer cycle, etc.)
    pub nesting_depth: usize,
}

/// Configuration for workgraph cycle iteration behavior.
/// This is the metadata that must be explicitly provided by the user —
/// it cannot be inferred from graph structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleIterationConfig {
    /// Hard cap on cycle iterations.
    pub max_iterations: u32,
    /// Current iteration (0 = first run).
    pub current_iteration: u32,
    /// Whether the cycle has converged (terminated early).
    pub converged: bool,
}

/// Extracts cycle metadata from detected SCCs.
///
/// Given a set of SCCs and the full adjacency list, computes:
/// - Which node is the header of each cycle
/// - Whether each cycle is reducible
/// - Back edges within each cycle
/// - Nesting depth
///
/// # Arguments
/// * `sccs` — non-trivial SCCs from `find_cycles()` or `tarjan_scc()`
/// * `num_nodes` — total number of nodes
/// * `adj` — full adjacency list
///
/// # Returns
/// A `CycleMetadata` for each SCC.
///
/// # Complexity
/// * Time: O(V + E) total across all SCCs
/// * Space: O(V)
///
/// # Example
/// ```
/// use workgraph::cycle::{find_cycles, extract_cycle_metadata};
///
/// // Graph: 0 → 1 → 2 → 0, with 3 → 0 (external entry)
/// let adj = vec![vec![1], vec![2], vec![0], vec![0]];
/// let sccs = find_cycles(4, &adj, false);
/// let metadata = extract_cycle_metadata(&sccs, 4, &adj);
/// assert_eq!(metadata.len(), 1);
/// assert_eq!(metadata[0].header, 0); // 0 has external predecessor (3)
/// assert!(metadata[0].reducible);
/// ```
pub fn extract_cycle_metadata(
    sccs: &[Scc],
    num_nodes: usize,
    adj: &[Vec<NodeId>],
) -> Vec<CycleMetadata> {
    // Build reverse adjacency list
    let mut rev_adj: Vec<Vec<NodeId>> = vec![Vec::new(); num_nodes];
    for (u, succs) in adj.iter().enumerate() {
        for &v in succs {
            rev_adj[v].push(u);
        }
    }

    // Build node-to-SCC mapping
    let mut node_to_scc: HashMap<NodeId, usize> = HashMap::new();
    for (i, scc) in sccs.iter().enumerate() {
        for &node in &scc.members {
            node_to_scc.insert(node, i);
        }
    }

    let mut result = Vec::new();

    for (scc_idx, scc) in sccs.iter().enumerate() {
        let member_set: HashSet<NodeId> = scc.members.iter().copied().collect();

        // Find entry nodes: nodes with predecessors outside this SCC
        let mut entry_nodes: Vec<NodeId> = Vec::new();
        for &node in &scc.members {
            let has_external_pred = rev_adj[node]
                .iter()
                .any(|&pred| !member_set.contains(&pred));
            if has_external_pred {
                entry_nodes.push(node);
            }
        }

        let (header, reducible) = match entry_nodes.len() {
            0 => {
                // Isolated cycle — pick smallest ID
                let mut sorted = scc.members.clone();
                sorted.sort_unstable();
                (sorted[0], true)
            }
            1 => (entry_nodes[0], true),
            _ => {
                // Multiple entry points — irreducible
                let mut sorted = entry_nodes;
                sorted.sort_unstable();
                (sorted[0], false)
            }
        };

        // Identify back edges: edges within the SCC that point to the header
        let mut back_edges = Vec::new();
        for &pred in &rev_adj[header] {
            if member_set.contains(&pred) {
                back_edges.push((pred, header));
            }
        }

        // Compute nesting depth: how many other SCCs contain this SCC's header
        let mut nesting_depth = 0;
        for (other_idx, other_scc) in sccs.iter().enumerate() {
            if other_idx == scc_idx {
                continue;
            }
            let other_set: HashSet<NodeId> = other_scc.members.iter().copied().collect();
            if other_set.contains(&header) {
                nesting_depth += 1;
            }
        }

        let mut members = scc.members.clone();
        members.sort_unstable();

        result.push(CycleMetadata {
            members,
            header,
            reducible,
            back_edges,
            nesting_depth,
        });
    }

    result
}

/// Convenience function: analyze a graph completely.
///
/// Runs Tarjan's SCC, filters to cycles, and extracts metadata — all in one call.
///
/// # Arguments
/// * `num_nodes` — total number of nodes
/// * `adj` — adjacency list
///
/// # Returns
/// Complete cycle metadata for all detected cycles.
///
/// # Complexity
/// * Time: O(V + E)
/// * Space: O(V)
pub fn analyze_graph_cycles(
    num_nodes: usize,
    adj: &[Vec<NodeId>],
) -> Vec<CycleMetadata> {
    let sccs = find_cycles(num_nodes, adj, false);
    extract_cycle_metadata(&sccs, num_nodes, adj)
}

// ─────────────────────────────────────────────────────────────────────────────
// Named Graph Helper (for tests and workgraph integration)
// ─────────────────────────────────────────────────────────────────────────────

/// A graph with string-named nodes, providing a convenient API for
/// building graphs and mapping between string IDs and numeric NodeIds.
///
/// This bridges the gap between workgraph's string-based task IDs and
/// the numeric IDs used by the cycle detection algorithms.
#[derive(Debug, Clone)]
pub struct NamedGraph {
    names: Vec<String>,
    name_to_id: HashMap<String, NodeId>,
    adj: Vec<Vec<NodeId>>,
}

impl Default for NamedGraph {
    fn default() -> Self {
        Self::new()
    }
}

impl NamedGraph {
    /// Creates an empty named graph.
    pub fn new() -> Self {
        Self {
            names: Vec::new(),
            name_to_id: HashMap::new(),
            adj: Vec::new(),
        }
    }

    /// Adds a node with the given name. Returns the NodeId.
    /// If the node already exists, returns its existing ID.
    pub fn add_node(&mut self, name: &str) -> NodeId {
        if let Some(&id) = self.name_to_id.get(name) {
            return id;
        }
        let id = self.names.len();
        self.names.push(name.to_string());
        self.name_to_id.insert(name.to_string(), id);
        self.adj.push(Vec::new());
        id
    }

    /// Adds a directed edge from `from` to `to` (by name).
    /// Adds nodes if they don't exist.
    pub fn add_edge(&mut self, from: &str, to: &str) {
        let from_id = self.add_node(from);
        let to_id = self.add_node(to);
        self.adj[from_id].push(to_id);
    }

    /// Returns the numeric ID for a name, if it exists.
    pub fn get_id(&self, name: &str) -> Option<NodeId> {
        self.name_to_id.get(name).copied()
    }

    /// Returns the name for a numeric ID.
    pub fn get_name(&self, id: NodeId) -> &str {
        &self.names[id]
    }

    /// Returns the number of nodes.
    pub fn num_nodes(&self) -> usize {
        self.names.len()
    }

    /// Returns a reference to the adjacency list.
    pub fn adjacency(&self) -> &[Vec<NodeId>] {
        &self.adj
    }

    /// Runs full cycle analysis and returns metadata with string names.
    pub fn analyze_cycles(&self) -> Vec<CycleMetadata> {
        analyze_graph_cycles(self.num_nodes(), &self.adj)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─────────────────────────────────────────────────────
    // Tarjan's SCC Tests
    // ─────────────────────────────────────────────────────

    #[test]
    fn test_empty_graph() {
        let sccs = tarjan_scc(0, &[]);
        assert!(sccs.is_empty());
    }

    #[test]
    fn test_single_node_no_cycle() {
        let adj = vec![vec![]];
        let sccs = tarjan_scc(1, &adj);
        assert_eq!(sccs.len(), 1);
        assert_eq!(sccs[0].members.len(), 1);
        // Single-node SCC without self-loop is not a cycle
        let cycles = find_cycles(1, &adj, false);
        assert!(cycles.is_empty());
    }

    #[test]
    fn test_single_node_self_loop() {
        let adj = vec![vec![0]];
        let cycles = find_cycles(1, &adj, true);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members, vec![0]);
    }

    #[test]
    fn test_self_loop_excluded_by_default() {
        let adj = vec![vec![0]];
        let cycles = find_cycles(1, &adj, false);
        assert!(cycles.is_empty());
    }

    #[test]
    fn test_two_nodes_no_cycle() {
        // 0 → 1 (no cycle)
        let adj = vec![vec![1], vec![]];
        let cycles = find_cycles(2, &adj, false);
        assert!(cycles.is_empty());
    }

    #[test]
    fn test_two_node_cycle() {
        // 0 → 1 → 0
        let adj = vec![vec![1], vec![0]];
        let cycles = find_cycles(2, &adj, false);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 2);
        let mut members = cycles[0].members.clone();
        members.sort();
        assert_eq!(members, vec![0, 1]);
    }

    #[test]
    fn test_simple_three_node_cycle() {
        // 0 → 1 → 2 → 0
        let adj = vec![vec![1], vec![2], vec![0]];
        let cycles = find_cycles(3, &adj, false);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 3);
        let mut members = cycles[0].members.clone();
        members.sort();
        assert_eq!(members, vec![0, 1, 2]);
    }

    #[test]
    fn test_complex_cycle_ten_nodes() {
        // 0 → 1 → 2 → ... → 9 → 0
        let adj: Vec<Vec<NodeId>> = (0..10).map(|i| vec![(i + 1) % 10]).collect();
        let cycles = find_cycles(10, &adj, false);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 10);
    }

    #[test]
    fn test_multiple_disjoint_cycles() {
        // Cycle 1: 0 → 1 → 0
        // Cycle 2: 2 → 3 → 4 → 2
        // Node 5: no cycle
        let adj = vec![
            vec![1],    // 0 → 1
            vec![0],    // 1 → 0
            vec![3],    // 2 → 3
            vec![4],    // 3 → 4
            vec![2],    // 4 → 2
            vec![],     // 5 (isolated)
        ];
        let cycles = find_cycles(6, &adj, false);
        assert_eq!(cycles.len(), 2);

        let mut cycle_sizes: Vec<usize> = cycles.iter().map(|c| c.members.len()).collect();
        cycle_sizes.sort();
        assert_eq!(cycle_sizes, vec![2, 3]);
    }

    #[test]
    fn test_nested_cycles_single_scc() {
        // 0 → 1 → 2 → 0 (outer)
        // 1 → 3 → 1     (inner, shared node 1)
        // Nodes 0,1,2,3 all in one SCC because they're mutually reachable
        let adj = vec![
            vec![1],       // 0 → 1
            vec![2, 3],    // 1 → 2, 1 → 3
            vec![0],       // 2 → 0
            vec![1],       // 3 → 1
        ];
        let sccs = find_cycles(4, &adj, false);
        // All 4 nodes form one SCC
        assert_eq!(sccs.len(), 1);
        assert_eq!(sccs[0].members.len(), 4);
    }

    #[test]
    fn test_nested_cycles_separate_sccs() {
        // Outer: 0 → 1 → 2 → 0
        // Inner: 3 → 4 → 3 (completely separate)
        // Connection: 1 → 3 (one-way, so inner is separate SCC)
        let adj = vec![
            vec![1],       // 0 → 1
            vec![2, 3],    // 1 → 2, 1 → 3
            vec![0],       // 2 → 0
            vec![4],       // 3 → 4
            vec![3],       // 4 → 3
        ];
        let sccs = find_cycles(5, &adj, false);
        assert_eq!(sccs.len(), 2);
    }

    #[test]
    fn test_linear_chain_no_cycle() {
        // 0 → 1 → 2 → 3 → 4
        let adj = vec![vec![1], vec![2], vec![3], vec![4], vec![]];
        let cycles = find_cycles(5, &adj, false);
        assert!(cycles.is_empty());
    }

    #[test]
    fn test_diamond_no_cycle() {
        // 0 → 1, 0 → 2, 1 → 3, 2 → 3
        let adj = vec![vec![1, 2], vec![3], vec![3], vec![]];
        let cycles = find_cycles(4, &adj, false);
        assert!(cycles.is_empty());
    }

    #[test]
    fn test_large_cycle_1000_nodes() {
        // Ring of 1000 nodes
        let n = 1000;
        let adj: Vec<Vec<NodeId>> = (0..n).map(|i| vec![(i + 1) % n]).collect();
        let cycles = find_cycles(n, &adj, false);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), n);
    }

    #[test]
    fn test_tarjan_deterministic() {
        // Same graph, multiple runs → same result
        let adj = vec![vec![1, 2], vec![2], vec![0], vec![4], vec![3]];
        let r1 = tarjan_scc(5, &adj);
        let r2 = tarjan_scc(5, &adj);
        assert_eq!(r1, r2);
    }

    // ─────────────────────────────────────────────────────
    // Havlak Loop Nesting Forest Tests
    // ─────────────────────────────────────────────────────

    #[test]
    fn test_havlak_no_loops() {
        // 0 → 1 → 2 (linear)
        let adj = vec![vec![1], vec![2], vec![]];
        let forest = build_loop_nesting_forest(3, &adj, 0);
        assert!(forest.loops.is_empty());
        assert!(forest.roots.is_empty());
    }

    #[test]
    fn test_havlak_simple_loop() {
        // 0 → 1 → 2 → 1 (loop at 1-2, entered from 0)
        let adj = vec![vec![1], vec![2], vec![1]];
        let forest = build_loop_nesting_forest(3, &adj, 0);
        assert_eq!(forest.loops.len(), 1);
        assert!(forest.loops.contains_key(&1));
        let loop_node = &forest.loops[&1];
        assert_eq!(loop_node.header, 1);
        assert!(loop_node.body.contains(&1));
        assert!(loop_node.body.contains(&2));
        assert!(loop_node.reducible);
        assert_eq!(loop_node.back_edges, vec![(2, 1)]);
    }

    #[test]
    fn test_havlak_three_node_cycle_with_entry() {
        // 0 → 1 → 2 → 3 → 1 (loop: 1→2→3→1, entry from 0)
        let adj = vec![vec![1], vec![2], vec![3], vec![1]];
        let forest = build_loop_nesting_forest(4, &adj, 0);
        assert_eq!(forest.loops.len(), 1);
        let loop_node = &forest.loops[&1];
        assert_eq!(loop_node.header, 1);
        assert_eq!(loop_node.body.len(), 3); // 1, 2, 3
        assert!(loop_node.reducible);
    }

    #[test]
    fn test_havlak_nested_loops() {
        // 0 → 1 → 2 → 3 → 1  (outer loop: 1→2→3→1)
        //          2 → 4 → 2  (inner loop: 2→4→2)
        let adj = vec![
            vec![1],       // 0 → 1
            vec![2],       // 1 → 2
            vec![3, 4],    // 2 → 3, 2 → 4
            vec![1],       // 3 → 1
            vec![2],       // 4 → 2
        ];
        let forest = build_loop_nesting_forest(5, &adj, 0);
        assert_eq!(forest.loops.len(), 2);

        // Outer loop header = 1
        assert!(forest.loops.contains_key(&1));
        let outer = &forest.loops[&1];
        assert_eq!(outer.header, 1);
        assert!(outer.parent.is_none());

        // Inner loop header = 2
        assert!(forest.loops.contains_key(&2));
        let inner = &forest.loops[&2];
        assert_eq!(inner.header, 2);
        assert_eq!(inner.parent, Some(1));
        assert_eq!(inner.depth, 1);
    }

    #[test]
    fn test_havlak_irreducible_loop() {
        // 0 → 1, 0 → 2, 1 → 2, 2 → 1
        // Both 1 and 2 can be entered from outside (0), making the loop irreducible
        let adj = vec![vec![1, 2], vec![2], vec![1]];
        let forest = build_loop_nesting_forest(3, &adj, 0);
        // There should be a loop, and it should be marked irreducible
        assert!(!forest.loops.is_empty());
        let loop_node = forest.loops.values().next().unwrap();
        assert!(!loop_node.reducible);
    }

    #[test]
    fn test_havlak_self_loop() {
        // 0 → 1 → 1 → 2 (self-loop at 1)
        let adj = vec![vec![1], vec![1, 2], vec![]];
        let forest = build_loop_nesting_forest(3, &adj, 0);
        assert_eq!(forest.loops.len(), 1);
        assert!(forest.loops.contains_key(&1));
        let loop_node = &forest.loops[&1];
        assert_eq!(loop_node.header, 1);
        assert!(loop_node.body.contains(&1));
        assert_eq!(loop_node.back_edges, vec![(1, 1)]);
    }

    #[test]
    fn test_havlak_disconnected_nodes() {
        // 0 → 1 → 0, 2 is disconnected from entry
        let adj = vec![vec![1], vec![0], vec![]];
        let forest = build_loop_nesting_forest(3, &adj, 0);
        // Loop at 0→1→0
        assert_eq!(forest.loops.len(), 1);
        // Node 2 should not be in any loop
        assert!(!forest.node_to_loop.contains_key(&2));
    }

    // ─────────────────────────────────────────────────────
    // Incremental Cycle Detection Tests
    // ─────────────────────────────────────────────────────

    #[test]
    fn test_incremental_no_cycle() {
        let mut detector = IncrementalCycleDetector::new(3);
        assert!(detector.add_edge(0, 1).is_ok());
        assert!(detector.add_edge(1, 2).is_ok());
    }

    #[test]
    fn test_incremental_detects_cycle() {
        let mut detector = IncrementalCycleDetector::new(3);
        assert!(detector.add_edge(0, 1).is_ok());
        assert!(detector.add_edge(1, 2).is_ok());
        let result = detector.add_edge(2, 0);
        assert!(result.is_err());
        let cycle = result.unwrap_err();
        assert_eq!(cycle.len(), 3);
    }

    #[test]
    fn test_incremental_self_loop() {
        let mut detector = IncrementalCycleDetector::new(2);
        let result = detector.add_edge(0, 0);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), vec![0]);
    }

    #[test]
    fn test_incremental_add_edges_one_by_one() {
        // Build a graph incrementally, verify detection at each step
        let mut detector = IncrementalCycleDetector::new(5);

        // 0 → 1
        assert!(detector.add_edge(0, 1).is_ok());
        // 1 → 2
        assert!(detector.add_edge(1, 2).is_ok());
        // 2 → 3
        assert!(detector.add_edge(2, 3).is_ok());
        // 3 → 4 (all fine, linear chain)
        assert!(detector.add_edge(3, 4).is_ok());
        // 4 → 2 (creates cycle 2→3→4→2)
        let result = detector.add_edge(4, 2);
        assert!(result.is_err());
        let cycle = result.unwrap_err();
        assert!(cycle.len() >= 3);
    }

    #[test]
    fn test_incremental_from_acyclic() {
        let adj = vec![vec![1], vec![2], vec![]];
        let mut detector = IncrementalCycleDetector::from_acyclic(3, adj);

        // Adding edge 2→0 creates a cycle
        let result = detector.add_edge(2, 0);
        assert!(result.is_err());
    }

    #[test]
    fn test_incremental_multiple_independent_paths() {
        let mut detector = IncrementalCycleDetector::new(4);
        // 0 → 1, 0 → 2, 1 → 3, 2 → 3 (diamond, no cycle)
        assert!(detector.add_edge(0, 1).is_ok());
        assert!(detector.add_edge(0, 2).is_ok());
        assert!(detector.add_edge(1, 3).is_ok());
        assert!(detector.add_edge(2, 3).is_ok());
        // 3 → 0 would create a cycle
        assert!(detector.add_edge(3, 0).is_err());
        // But 3 → 1 would also create a cycle (via 1→3→1)
        // (detector already rejected 3→0, so state is unchanged for 3→1)
        let mut detector2 = IncrementalCycleDetector::new(4);
        assert!(detector2.add_edge(0, 1).is_ok());
        assert!(detector2.add_edge(0, 2).is_ok());
        assert!(detector2.add_edge(1, 3).is_ok());
        assert!(detector2.add_edge(2, 3).is_ok());
        assert!(detector2.add_edge(3, 1).is_err());
    }

    // ─────────────────────────────────────────────────────
    // check_edge_addition Tests
    // ─────────────────────────────────────────────────────

    #[test]
    fn test_check_edge_no_cycle() {
        let adj = vec![vec![1], vec![2], vec![]];
        let result = check_edge_addition(3, &adj, 0, 2);
        assert_eq!(result, EdgeAddResult::NoCycle);
    }

    #[test]
    fn test_check_edge_creates_cycle() {
        let adj = vec![vec![1], vec![2], vec![]];
        let result = check_edge_addition(3, &adj, 2, 0);
        match result {
            EdgeAddResult::CreatesCycle { cycle_members } => {
                assert_eq!(cycle_members.len(), 3);
                assert_eq!(cycle_members[0], 0);
                assert_eq!(cycle_members[2], 2);
            }
            _ => panic!("expected cycle"),
        }
    }

    #[test]
    fn test_check_edge_self_loop() {
        let adj = vec![vec![]];
        let result = check_edge_addition(1, &adj, 0, 0);
        match result {
            EdgeAddResult::CreatesCycle { cycle_members } => {
                assert_eq!(cycle_members, vec![0]);
            }
            _ => panic!("expected self-loop cycle"),
        }
    }

    // ─────────────────────────────────────────────────────
    // Cycle Metadata Extraction Tests
    // ─────────────────────────────────────────────────────

    #[test]
    fn test_metadata_simple_cycle_with_external_entry() {
        // 3 → 0 → 1 → 2 → 0 (cycle is 0,1,2 with external entry from 3)
        let adj = vec![
            vec![1],    // 0 → 1
            vec![2],    // 1 → 2
            vec![0],    // 2 → 0
            vec![0],    // 3 → 0 (external)
        ];
        let metadata = analyze_graph_cycles(4, &adj);
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].header, 0);
        assert!(metadata[0].reducible);
        assert_eq!(metadata[0].members.len(), 3);
        assert!(!metadata[0].back_edges.is_empty());
        // Back edge should be (2, 0) — 2 points to header 0
        assert!(metadata[0].back_edges.contains(&(2, 0)));
    }

    #[test]
    fn test_metadata_isolated_cycle_picks_smallest() {
        // 3 → 1 → 2 → 3 (no external entry — isolated cycle)
        // But nodes numbered 1,2,3 — smallest = 1
        let adj = vec![
            vec![],     // 0 (isolated)
            vec![2],    // 1 → 2
            vec![3],    // 2 → 3
            vec![1],    // 3 → 1
        ];
        let metadata = analyze_graph_cycles(4, &adj);
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].header, 1); // smallest in cycle
        assert!(metadata[0].reducible);
    }

    #[test]
    fn test_metadata_irreducible_cycle() {
        // 0 → 1, 0 → 2, 1 → 2, 2 → 1
        // Both 1 and 2 have external predecessors (0)
        let adj = vec![
            vec![1, 2], // 0 → 1, 0 → 2
            vec![2],    // 1 → 2
            vec![1],    // 2 → 1
        ];
        let metadata = analyze_graph_cycles(3, &adj);
        assert_eq!(metadata.len(), 1);
        assert!(!metadata[0].reducible);
        // Header should be the smallest entry node = 1
        assert_eq!(metadata[0].header, 1);
    }

    #[test]
    fn test_metadata_multiple_disjoint_cycles() {
        // Cycle 1: 0 → 1 → 0
        // Cycle 2: 2 → 3 → 4 → 2
        let adj = vec![
            vec![1],    // 0 → 1
            vec![0],    // 1 → 0
            vec![3],    // 2 → 3
            vec![4],    // 3 → 4
            vec![2],    // 4 → 2
        ];
        let metadata = analyze_graph_cycles(5, &adj);
        assert_eq!(metadata.len(), 2);
    }

    #[test]
    fn test_metadata_no_cycles() {
        let adj = vec![vec![1], vec![2], vec![]];
        let metadata = analyze_graph_cycles(3, &adj);
        assert!(metadata.is_empty());
    }

    // ─────────────────────────────────────────────────────
    // NamedGraph Tests
    // ─────────────────────────────────────────────────────

    #[test]
    fn test_named_graph_basic() {
        let mut g = NamedGraph::new();
        g.add_edge("write", "review");
        g.add_edge("review", "revise");
        g.add_edge("revise", "write");

        let cycles = g.analyze_cycles();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 3);
    }

    #[test]
    fn test_named_graph_with_external_entry() {
        let mut g = NamedGraph::new();
        g.add_node("spec");
        g.add_edge("spec", "write");
        g.add_edge("write", "review");
        g.add_edge("review", "write");

        let cycles = g.analyze_cycles();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 2);
        // Header should be "write" (has external predecessor "spec")
        let write_id = g.get_id("write").unwrap();
        assert_eq!(cycles[0].header, write_id);
    }

    // ─────────────────────────────────────────────────────
    // Performance Tests
    // ─────────────────────────────────────────────────────

    #[test]
    fn test_performance_1000_node_graph() {
        // 1000-node graph with multiple cycles
        let n = 1000;
        let mut adj: Vec<Vec<NodeId>> = vec![Vec::new(); n];

        // Create a few cycles of various sizes
        // Cycle 1: 0..10
        for i in 0..10 {
            adj[i].push((i + 1) % 10);
        }
        // Cycle 2: 100..120
        for i in 100..120 {
            let next = if i == 119 { 100 } else { i + 1 };
            adj[i].push(next);
        }
        // Cycle 3: 500..510
        for i in 500..510 {
            let next = if i == 509 { 500 } else { i + 1 };
            adj[i].push(next);
        }
        // Some linear chains connecting things
        for i in 10..100 {
            adj[i].push(i + 1);
        }
        for i in 200..500 {
            adj[i].push(i + 1);
        }

        let start = std::time::Instant::now();
        let cycles = find_cycles(n, &adj, false);
        let elapsed = start.elapsed();

        assert_eq!(cycles.len(), 3);
        assert!(
            elapsed.as_millis() < 10,
            "Tarjan SCC took {}ms, expected < 10ms",
            elapsed.as_millis()
        );
    }

    #[test]
    fn test_performance_incremental_1000_nodes() {
        let n = 1000;
        let mut detector = IncrementalCycleDetector::new(n);

        let start = std::time::Instant::now();

        // Add a long chain: 0→1→2→...→999
        for i in 0..n - 1 {
            assert!(detector.add_edge(i, i + 1).is_ok());
        }

        // Try to close the cycle 999→0 (should detect)
        let result = detector.add_edge(999, 0);
        assert!(result.is_err());

        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 10,
            "Incremental detection took {}ms, expected < 10ms",
            elapsed.as_millis()
        );
    }

    // ─────────────────────────────────────────────────────
    // Edge Cases and Property-Style Tests
    // ─────────────────────────────────────────────────────

    #[test]
    fn test_all_nodes_in_one_scc() {
        // Complete graph on 5 nodes → one big SCC
        let n = 5;
        let adj: Vec<Vec<NodeId>> = (0..n)
            .map(|i| (0..n).filter(|&j| j != i).collect())
            .collect();
        let sccs = find_cycles(n, &adj, false);
        assert_eq!(sccs.len(), 1);
        assert_eq!(sccs[0].members.len(), 5);
    }

    #[test]
    fn test_two_node_mutual() {
        // 0 ↔ 1 (mutual edges)
        let adj = vec![vec![1], vec![0]];
        let cycles = find_cycles(2, &adj, false);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 2);
    }

    #[test]
    fn test_graph_with_dead_ends_and_cycle() {
        // 0 → 1 → 2 → 0 (cycle)
        // 0 → 3 (dead end)
        // 4 → 1 (joins into cycle)
        let adj = vec![
            vec![1, 3],    // 0 → 1, 0 → 3
            vec![2],       // 1 → 2
            vec![0],       // 2 → 0
            vec![],        // 3 (dead end)
            vec![1],       // 4 → 1
        ];
        let cycles = find_cycles(5, &adj, false);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 3);
    }

    #[test]
    fn test_multiple_back_edges_same_scc() {
        // 0 → 1 → 2 → 0 (back edge 2→0)
        // 1 → 0           (additional back edge 1→0)
        let adj = vec![
            vec![1],       // 0 → 1
            vec![2, 0],    // 1 → 2, 1 → 0
            vec![0],       // 2 → 0
        ];
        let cycles = find_cycles(3, &adj, false);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 3);

        let metadata = analyze_graph_cycles(3, &adj);
        assert_eq!(metadata[0].back_edges.len(), 2); // Both 1→0 and 2→0
    }

    #[test]
    fn test_long_tail_then_cycle() {
        // 0 → 1 → 2 → 3 → 4 → 5 → 3 (cycle 3→4→5→3, tail 0→1→2)
        let adj = vec![
            vec![1],    // 0 → 1
            vec![2],    // 1 → 2
            vec![3],    // 2 → 3
            vec![4],    // 3 → 4
            vec![5],    // 4 → 5
            vec![3],    // 5 → 3
        ];
        let cycles = find_cycles(6, &adj, false);
        assert_eq!(cycles.len(), 1);
        let mut members = cycles[0].members.clone();
        members.sort();
        assert_eq!(members, vec![3, 4, 5]);
    }

    #[test]
    fn test_figure_eight_two_cycles_sharing_node() {
        // 0 → 1 → 2 → 0 (cycle A)
        // 0 → 3 → 4 → 0 (cycle B)
        // Node 0 is shared — both cycles merge into one SCC
        let adj = vec![
            vec![1, 3],    // 0 → 1, 0 → 3
            vec![2],       // 1 → 2
            vec![0],       // 2 → 0
            vec![4],       // 3 → 4
            vec![0],       // 4 → 0
        ];
        let cycles = find_cycles(5, &adj, false);
        // All 5 nodes form one SCC since 0 connects both cycles
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 5);
    }

    #[test]
    fn test_parallel_edges() {
        // 0 → 1, 0 → 1, 1 → 0 (duplicate edge, cycle)
        let adj = vec![vec![1, 1], vec![0]];
        let cycles = find_cycles(2, &adj, false);
        assert_eq!(cycles.len(), 1);
    }

    #[test]
    fn test_forest_nesting_depth_three_levels() {
        // Level 0: 0 → 1 → 2 → 3 → 1  (outer loop: header=1)
        //   Level 1: 2 → 4 → 5 → 2      (middle loop: header=2)
        //     Level 2: 4 → 6 → 4          (inner loop: header=4)
        let adj = vec![
            vec![1],           // 0 → 1
            vec![2],           // 1 → 2
            vec![3, 4],        // 2 → 3, 2 → 4
            vec![1],           // 3 → 1
            vec![5, 6],        // 4 → 5, 4 → 6
            vec![2],           // 5 → 2
            vec![4],           // 6 → 4
        ];
        let forest = build_loop_nesting_forest(7, &adj, 0);
        assert_eq!(forest.loops.len(), 3);

        assert_eq!(forest.loops[&1].depth, 0);
        assert_eq!(forest.loops[&2].depth, 1);
        assert_eq!(forest.loops[&4].depth, 2);

        assert_eq!(forest.loops[&2].parent, Some(1));
        assert_eq!(forest.loops[&4].parent, Some(2));
    }

    #[test]
    fn test_analyze_graph_cycles_convenience() {
        let adj = vec![vec![1], vec![2], vec![0]];
        let metadata = analyze_graph_cycles(3, &adj);
        assert_eq!(metadata.len(), 1);
        assert_eq!(metadata[0].members.len(), 3);
    }

    #[test]
    fn test_cycle_metadata_nesting_depth() {
        // When two SCCs share nodes (which means they merge into one SCC),
        // nesting depth should be 0 since SCC-level nesting doesn't happen
        // in a single SCC. True nesting requires separate SCCs.
        let adj = vec![
            vec![1],    // 0 → 1
            vec![2],    // 1 → 2
            vec![0],    // 2 → 0 (cycle: 0,1,2)
            vec![4],    // 3 → 4
            vec![3],    // 4 → 3 (cycle: 3,4)
        ];
        let metadata = analyze_graph_cycles(5, &adj);
        assert_eq!(metadata.len(), 2);
        // Both should have nesting depth 0 (independent cycles)
        for m in &metadata {
            assert_eq!(m.nesting_depth, 0);
        }
    }

    // ─────────────────────────────────────────────────────
    // Workgraph-Specific Scenario Tests
    // ─────────────────────────────────────────────────────

    #[test]
    fn test_review_revise_cycle() {
        // Typical workgraph pattern: write → review → revise → write
        let mut g = NamedGraph::new();
        g.add_edge("write", "review");
        g.add_edge("review", "revise");
        g.add_edge("revise", "write");

        let cycles = g.analyze_cycles();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].members.len(), 3);
        assert!(cycles[0].reducible); // No external entry — isolated but reducible
    }

    #[test]
    fn test_ci_retry_cycle() {
        // CI pattern: build → test → deploy → monitor → build
        let mut g = NamedGraph::new();
        g.add_node("spec");
        g.add_edge("spec", "build");
        g.add_edge("build", "test");
        g.add_edge("test", "deploy");
        g.add_edge("deploy", "monitor");
        g.add_edge("monitor", "build");

        let cycles = g.analyze_cycles();
        assert_eq!(cycles.len(), 1);
        let build_id = g.get_id("build").unwrap();
        assert_eq!(cycles[0].header, build_id); // build has external pred (spec)
        assert!(cycles[0].reducible);
        assert_eq!(cycles[0].members.len(), 4);
    }

    #[test]
    fn test_incremental_edge_by_edge_verification() {
        // Build the review-revise cycle incrementally
        let mut detector = IncrementalCycleDetector::new(3);
        // write(0) → review(1)
        assert!(detector.add_edge(0, 1).is_ok());
        // review(1) → revise(2)
        assert!(detector.add_edge(1, 2).is_ok());
        // revise(2) → write(0) — creates cycle!
        let result = detector.add_edge(2, 0);
        assert!(result.is_err());
        let cycle = result.unwrap_err();
        assert!(cycle.contains(&0));
        assert!(cycle.contains(&1));
        assert!(cycle.contains(&2));
    }
}
