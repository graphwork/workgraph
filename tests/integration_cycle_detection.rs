//! Integration tests for cycle detection and structural cycle iteration.
//!
//! Tests cover:
//! 1. CycleAnalysis: detection, headers, back-edges, reducibility
//! 2. Dispatch: cycle-aware readiness via back-edge exemption
//! 3. Completion: evaluate_cycle_iteration re-opens cycle members
//! 4. Migration: backward compat with old loops_to JSONL format
//! 5. CLI: wg add --max-iterations, wg cycles, wg check

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{
    CycleConfig, LoopGuard, Node, Status, Task, WorkGraph,
    evaluate_cycle_iteration,
};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::{ready_tasks, ready_tasks_cycle_aware};

// ===========================================================================
// Helpers
// ===========================================================================

fn make_task(id: &str, title: &str) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        ..Task::default()
    }
}

fn make_task_with_status(id: &str, title: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        status,
        ..Task::default()
    }
}

fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("could not get current exe path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push("wg");
    assert!(
        path.exists(),
        "wg binary not found at {:?}. Run `cargo build` first.",
        path
    );
    path
}

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(wg_binary())
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn wg_ok(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg {:?} failed.\nstdout: {}\nstderr: {}",
        args,
        stdout,
        stderr
    );
    stdout
}

fn setup_workgraph(tmp: &TempDir, tasks: Vec<Task>) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

/// Build a graph from tasks without writing to disk.
fn build_graph(tasks: Vec<Task>) -> WorkGraph {
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    graph
}

// ===========================================================================
// 1. CycleAnalysis integration tests
// ===========================================================================

#[test]
fn test_cycle_analysis_empty_graph() {
    let graph = WorkGraph::new();
    let analysis = graph.compute_cycle_analysis();
    assert!(analysis.cycles.is_empty());
    assert!(analysis.task_to_cycle.is_empty());
    assert!(analysis.back_edges.is_empty());
}

#[test]
fn test_cycle_analysis_linear_chain_no_cycles() {
    let t1 = make_task("t1", "Task 1");
    let mut t2 = make_task("t2", "Task 2");
    t2.after = vec!["t1".to_string()];
    let mut t3 = make_task("t3", "Task 3");
    t3.after = vec!["t2".to_string()];

    let graph = build_graph(vec![t1, t2, t3]);
    let analysis = graph.compute_cycle_analysis();

    assert!(analysis.cycles.is_empty(), "Linear chain should have no cycles");
    assert!(analysis.task_to_cycle.is_empty());
}

#[test]
fn test_cycle_analysis_dag_no_cycles() {
    // Diamond DAG: A → B, A → C, B → D, C → D
    let a = make_task("a", "A");
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "C");
    c.after = vec!["a".to_string()];
    let mut d = make_task("d", "D");
    d.after = vec!["b".to_string(), "c".to_string()];

    let graph = build_graph(vec![a, b, c, d]);
    let analysis = graph.compute_cycle_analysis();

    assert!(analysis.cycles.is_empty(), "Diamond DAG should have no cycles");
}

#[test]
fn test_cycle_analysis_simple_two_node_cycle() {
    // A.after = [B], B.after = [A] → cycle A↔B
    let mut a = make_task("a", "Task A");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];

    let graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    assert_eq!(analysis.cycles.len(), 1, "Should detect exactly one cycle");
    let cycle = &analysis.cycles[0];
    assert_eq!(cycle.members.len(), 2, "Cycle should have 2 members");

    let members: HashSet<&str> = cycle.members.iter().map(|s| s.as_str()).collect();
    assert!(members.contains("a"));
    assert!(members.contains("b"));
}

#[test]
fn test_cycle_analysis_three_node_cycle() {
    // spec → impl → review → spec (spec.after=[review], impl.after=[spec], review.after=[impl])
    let mut spec = make_task("spec", "Specification");
    spec.after = vec!["review".to_string()];
    let mut imp = make_task("impl", "Implementation");
    imp.after = vec!["spec".to_string()];
    let mut review = make_task("review", "Review");
    review.after = vec!["impl".to_string()];

    let graph = build_graph(vec![spec, imp, review]);
    let analysis = graph.compute_cycle_analysis();

    assert_eq!(analysis.cycles.len(), 1, "Should detect exactly one 3-node cycle");
    let cycle = &analysis.cycles[0];
    assert_eq!(cycle.members.len(), 3, "Cycle should have 3 members");

    let members: HashSet<&str> = cycle.members.iter().map(|s| s.as_str()).collect();
    assert!(members.contains("spec"));
    assert!(members.contains("impl"));
    assert!(members.contains("review"));
}

#[test]
fn test_cycle_analysis_nested_cycles() {
    // Outer cycle: A → B → C → A
    // Inner cycle: B → D → B
    // B participates in both (they form a single SCC or two depending on structure)
    //
    // A.after = [C]
    // B.after = [A, D]
    // C.after = [B]
    // D.after = [B]
    let mut a = make_task("a", "Outer start");
    a.after = vec!["c".to_string()];
    let mut b = make_task("b", "Shared node");
    b.after = vec!["a".to_string(), "d".to_string()];
    let mut c = make_task("c", "Outer end");
    c.after = vec!["b".to_string()];
    let mut d = make_task("d", "Inner node");
    d.after = vec!["b".to_string()];

    let graph = build_graph(vec![a, b, c, d]);
    let analysis = graph.compute_cycle_analysis();

    // All four nodes form one large SCC since they're all interconnected
    assert!(
        !analysis.cycles.is_empty(),
        "Should detect cycles in nested structure"
    );

    // All four tasks should be in some cycle
    assert!(analysis.task_to_cycle.contains_key("a"));
    assert!(analysis.task_to_cycle.contains_key("b"));
    assert!(analysis.task_to_cycle.contains_key("c"));
    assert!(analysis.task_to_cycle.contains_key("d"));
}

#[test]
fn test_cycle_analysis_multiple_independent_cycles() {
    // Cycle 1: A ↔ B
    // Cycle 2: C ↔ D
    // No edges between the two cycles
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "C");
    c.after = vec!["d".to_string()];
    let mut d = make_task("d", "D");
    d.after = vec!["c".to_string()];
    // E is not in any cycle
    let e = make_task("e", "E");

    let graph = build_graph(vec![a, b, c, d, e]);
    let analysis = graph.compute_cycle_analysis();

    assert_eq!(
        analysis.cycles.len(),
        2,
        "Should detect exactly two independent cycles"
    );

    // E should not be in any cycle
    assert!(
        !analysis.task_to_cycle.contains_key("e"),
        "Non-cycle task should not be in task_to_cycle"
    );

    // A and B in the same cycle, C and D in the same cycle
    assert_eq!(analysis.task_to_cycle["a"], analysis.task_to_cycle["b"]);
    assert_eq!(analysis.task_to_cycle["c"], analysis.task_to_cycle["d"]);
    assert_ne!(analysis.task_to_cycle["a"], analysis.task_to_cycle["c"]);
}

#[test]
fn test_cycle_analysis_irreducible_cycle_detection() {
    // An irreducible cycle has multiple entry points.
    // X → A ↔ B ← Y where X and Y are external
    // A.after = [B, X], B.after = [A, Y]
    let x = make_task_with_status("x", "External X", Status::Done);
    let y = make_task_with_status("y", "External Y", Status::Done);
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string(), "x".to_string()];
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string(), "y".to_string()];

    let graph = build_graph(vec![x, y, a, b]);
    let analysis = graph.compute_cycle_analysis();

    assert_eq!(analysis.cycles.len(), 1, "Should detect one cycle");
    let cycle = &analysis.cycles[0];

    // The cycle has A and B as members
    let members: HashSet<&str> = cycle.members.iter().map(|s| s.as_str()).collect();
    assert!(members.contains("a"));
    assert!(members.contains("b"));

    // Both A and B have external predecessors → irreducible
    assert!(
        !cycle.reducible,
        "Cycle with multiple entry points should be irreducible"
    );
}

#[test]
fn test_cycle_analysis_reducible_with_external_entry() {
    // A has an external dep X; B and C do not.
    // X → A → B → C → A
    // A is the header because it's the only entry point.
    let x = make_task_with_status("x", "External", Status::Done);
    let mut a = make_task("a", "A (header)");
    a.after = vec!["c".to_string(), "x".to_string()];
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "C");
    c.after = vec!["b".to_string()];

    let graph = build_graph(vec![x, a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    assert_eq!(analysis.cycles.len(), 1);
    let cycle = &analysis.cycles[0];
    assert!(cycle.reducible, "Cycle with single entry point should be reducible");
    assert_eq!(
        cycle.header, "a",
        "A should be the header (only entry point from external)"
    );
}

#[test]
fn test_cycle_analysis_back_edges_identified() {
    // Cycle: A → B → C → A (A is header via external dep X)
    // A.after = [C, X], B.after = [A], C.after = [B]
    // Back-edge: C → A (the edge that closes the cycle)
    let x = make_task_with_status("x", "External", Status::Done);
    let mut a = make_task("a", "A");
    a.after = vec!["c".to_string(), "x".to_string()];
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "C");
    c.after = vec!["b".to_string()];

    let graph = build_graph(vec![x, a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    assert!(
        !analysis.back_edges.is_empty(),
        "Should identify back-edges"
    );
    // The back-edge should be (c, a) — c is the predecessor, a is the header
    assert!(
        analysis.back_edges.contains(&("c".to_string(), "a".to_string())),
        "Back-edge should be (c → a). Found: {:?}",
        analysis.back_edges
    );
}

#[test]
fn test_cycle_analysis_isolated_cycle_picks_header() {
    // Isolated cycle with no external entries: A ↔ B
    // Header should be deterministically chosen (smallest ID)
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];

    let graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    assert_eq!(analysis.cycles.len(), 1);
    let cycle = &analysis.cycles[0];
    // For an isolated cycle, header is determined by the algorithm
    // (entry-node heuristic or smallest ID). Just verify it's one of the members.
    assert!(
        cycle.header == "a" || cycle.header == "b",
        "Header should be a member of the cycle"
    );
}

#[test]
fn test_cycle_analysis_cache_invalidation() {
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);

    // First analysis: should find cycle
    let analysis1 = graph.get_cycle_analysis();
    assert_eq!(analysis1.cycles.len(), 1);

    // Add a new node (invalidates cache)
    let c = make_task("c", "C");
    graph.add_node(Node::Task(c));

    // Second analysis: cache was invalidated, recomputes
    let analysis2 = graph.get_cycle_analysis();
    // The A↔B cycle should still be there
    assert_eq!(analysis2.cycles.len(), 1);
}

#[test]
fn test_cycle_analysis_single_task_no_cycle() {
    let a = make_task("a", "A");
    let graph = build_graph(vec![a]);
    let analysis = graph.compute_cycle_analysis();
    assert!(analysis.cycles.is_empty(), "Single task should not form a cycle");
}

#[test]
fn test_cycle_analysis_deterministic() {
    let mut a = make_task("a", "A");
    a.after = vec!["c".to_string()];
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "C");
    c.after = vec!["b".to_string()];

    let graph = build_graph(vec![a, b, c]);

    // Run analysis multiple times; results should be identical
    let a1 = graph.compute_cycle_analysis();
    let a2 = graph.compute_cycle_analysis();
    let a3 = graph.compute_cycle_analysis();

    assert_eq!(a1.cycles.len(), a2.cycles.len());
    assert_eq!(a2.cycles.len(), a3.cycles.len());
    assert_eq!(a1.cycles[0].header, a2.cycles[0].header);
    assert_eq!(a2.cycles[0].header, a3.cycles[0].header);
    assert_eq!(a1.cycles[0].members, a2.cycles[0].members);
}

// ===========================================================================
// 2. Dispatch tests (cycle-aware readiness)
// ===========================================================================

#[test]
fn test_dispatch_header_ready_when_external_deps_done() {
    // Cycle: A → B → A. External dep X → A.
    // A.after = [B, X] (B is back-edge, X is external)
    // B.after = [A]
    // X is Done → A should be ready (back-edge from B exempt)
    let x = make_task_with_status("x", "External", Status::Done);
    let mut a = make_task("a", "A (header)");
    a.after = vec!["b".to_string(), "x".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];

    let graph = build_graph(vec![x, a, b]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"a"),
        "Header A should be ready (back-edge exempt, external X done). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_dispatch_header_not_ready_when_external_deps_open() {
    // Same as above but X is Open → A should NOT be ready
    let x = make_task("x", "External"); // Open status
    let mut a = make_task("a", "A (header)");
    a.after = vec!["b".to_string(), "x".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];

    let graph = build_graph(vec![x, a, b]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        !ready_ids.contains(&"a"),
        "Header A should NOT be ready (external X is open). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_dispatch_non_header_waits_for_predecessor() {
    // Cycle: A → B → A. A is header (Done). B should be ready.
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];

    let graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"b"),
        "B should be ready (A is Done). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_dispatch_non_header_not_ready_when_pred_open() {
    // Cycle: A → B → C → A. A is header (open, ready via exemption).
    // B.after = [A]. Since A is Open (not terminal), B should NOT be ready.
    let mut a = make_task("a", "A (header)");
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "C");
    c.after = vec!["b".to_string()];

    let graph = build_graph(vec![a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        !ready_ids.contains(&"b"),
        "B should NOT be ready (A is Open). Ready: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"c"),
        "C should NOT be ready (B is Open). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_dispatch_header_without_config_not_exempt() {
    // Cycle: A → B → A. A has NO cycle_config.
    // Back-edge exemption requires cycle_config on header.
    let mut a = make_task("a", "A (no config)");
    a.after = vec!["b".to_string()];
    // No cycle_config!
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];

    let graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    // Without cycle_config, back-edge exemption doesn't apply → deadlock
    assert!(
        ready_ids.is_empty(),
        "No tasks should be ready (unconfigured cycle → deadlock). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_dispatch_back_edge_exemption_only_for_header() {
    // Cycle: A → B → C → A. A is header (via external dep X → A).
    // Only A gets back-edge exemption. B and C do NOT.
    let x = make_task_with_status("x", "External", Status::Done);
    let mut a = make_task("a", "A (header)");
    a.after = vec!["c".to_string(), "x".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "C");
    c.after = vec!["b".to_string()];

    let graph = build_graph(vec![x, a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    // Only A should be ready (back-edge exempt, external X done)
    assert!(ready_ids.contains(&"a"), "A should be ready. Ready: {:?}", ready_ids);
    assert!(!ready_ids.contains(&"b"), "B should NOT be ready. Ready: {:?}", ready_ids);
    assert!(!ready_ids.contains(&"c"), "C should NOT be ready. Ready: {:?}", ready_ids);
}

#[test]
fn test_dispatch_non_cycle_tasks_unaffected() {
    // Non-cycle tasks should work normally with cycle_aware dispatch
    let x = make_task_with_status("x", "Done dep", Status::Done);
    let mut y = make_task("y", "Open task");
    y.after = vec!["x".to_string()];
    let z = make_task("z", "Independent");

    let graph = build_graph(vec![x, y, z]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(ready_ids.contains(&"y"), "Y should be ready (X is done)");
    assert!(ready_ids.contains(&"z"), "Z should be ready (no deps)");
}

#[test]
fn test_dispatch_reiteration_header_ready_after_reopen() {
    // After cycle re-opens, the header should again be ready (back-edge exempt)
    // Simulate: all cycle members re-opened (Open status, loop_iteration > 0)
    // Use external dep to ensure A is deterministically the header.
    let x = make_task_with_status("x", "External", Status::Done);
    let mut a = make_task("a", "A (header)");
    a.after = vec!["b".to_string(), "x".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
    });
    a.loop_iteration = 1; // Second iteration
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    b.loop_iteration = 1;

    let graph = build_graph(vec![x, a, b]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"a"),
        "Header A should be ready on re-iteration. Ready: {:?}",
        ready_ids
    );
}

// ===========================================================================
// 3. Completion and re-opening tests
// ===========================================================================

#[test]
fn test_completion_all_done_triggers_iteration() {
    // Cycle: A → B → A. A is header with max_iterations=3.
    // Both A and B are Done → should re-open both.
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        !reactivated.is_empty(),
        "Should re-activate cycle members when all done"
    );
    let reactivated_set: HashSet<&str> = reactivated.iter().map(|s| s.as_str()).collect();
    assert!(reactivated_set.contains("a"));
    assert!(reactivated_set.contains("b"));

    // Verify tasks are now Open
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
}

#[test]
fn test_completion_partial_no_iteration() {
    // Cycle: A → B → C → A. A is header.
    // A=Done, B=Done, C=Open → should NOT re-open.
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "C"); // Open
    c.after = vec!["b".to_string()];

    let mut graph = build_graph(vec![a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        reactivated.is_empty(),
        "Should NOT iterate when not all members are Done. Reactivated: {:?}",
        reactivated
    );
}

#[test]
fn test_completion_converged_stops_iteration() {
    // Cycle: A → B → A. A is header with "converged" tag.
    // Both Done, but converged tag prevents re-opening.
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    a.tags = vec!["converged".to_string()];
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        reactivated.is_empty(),
        "Should NOT iterate when header has 'converged' tag"
    );
    // Tasks should remain Done
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
}

#[test]
fn test_completion_max_iterations_respected() {
    // Cycle: A → B → A. max_iterations = 2, current iteration = 2.
    // Should NOT re-open (at max).
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 2,
        guard: None,
        delay: None,
    });
    a.loop_iteration = 2; // Already at max
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    b.loop_iteration = 2;

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        reactivated.is_empty(),
        "Should NOT iterate when at max_iterations"
    );
}

#[test]
fn test_completion_max_iterations_allows_under_limit() {
    // Cycle: A → B → A. max_iterations = 3, current iteration = 1.
    // Should re-open (under limit).
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    a.loop_iteration = 1;
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    b.loop_iteration = 1;

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        !reactivated.is_empty(),
        "Should iterate (under max_iterations)"
    );
    // Iteration should increment
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 2);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 2);
}

#[test]
fn test_completion_guard_prevents_iteration() {
    // Guard: task:sentinel=failed. Sentinel is Done (not Failed) → guard blocks.
    let sentinel = make_task_with_status("sentinel", "Sentinel", Status::Done);
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: Some(LoopGuard::TaskStatus {
            task: "sentinel".to_string(),
            status: Status::Failed,
        }),
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![sentinel, a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        reactivated.is_empty(),
        "Guard should prevent iteration (sentinel not failed)"
    );
}

#[test]
fn test_completion_guard_allows_iteration() {
    // Guard: task:sentinel=failed. Sentinel IS Failed → guard allows.
    let sentinel = make_task_with_status("sentinel", "Sentinel", Status::Failed);
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: Some(LoopGuard::TaskStatus {
            task: "sentinel".to_string(),
            status: Status::Failed,
        }),
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![sentinel, a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        !reactivated.is_empty(),
        "Guard should allow iteration (sentinel is failed)"
    );
}

#[test]
fn test_completion_guard_always_allows() {
    // Guard: Always → always iterate (up to max_iterations)
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: Some(LoopGuard::Always),
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(!reactivated.is_empty(), "Always guard should allow iteration");
}

#[test]
fn test_completion_delay_applied() {
    // Cycle with delay: header should have ready_after set after re-opening
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: Some("30s".to_string()),
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(!reactivated.is_empty(), "Should iterate with delay");

    // Header should have ready_after set
    let header = graph.get_task("a").unwrap();
    assert!(
        header.ready_after.is_some(),
        "Header should have ready_after set for delay"
    );

    // Non-header member should NOT have ready_after
    let member = graph.get_task("b").unwrap();
    assert!(
        member.ready_after.is_none(),
        "Non-header should not have ready_after"
    );
}

#[test]
fn test_completion_no_config_no_iteration() {
    // Cycle without CycleConfig → no iteration (one-shot)
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    // No cycle_config!
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        reactivated.is_empty(),
        "Should NOT iterate without CycleConfig"
    );
}

#[test]
fn test_completion_iteration_counter_increments() {
    // Run two iterations and verify counter increments correctly
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);

    // First iteration
    let analysis = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(!reactivated.is_empty());
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 1);

    // Set both back to Done for second iteration
    graph.get_task_mut("a").unwrap().status = Status::Done;
    graph.get_task_mut("b").unwrap().status = Status::Done;

    // Second iteration
    let analysis = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(!reactivated.is_empty());
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 2);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 2);
}

#[test]
fn test_completion_clears_assignment_and_timestamps() {
    // Verify re-opening clears assigned, started_at, completed_at
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    a.assigned = Some("agent-1".to_string());
    a.started_at = Some("2026-01-01T00:00:00Z".to_string());
    a.completed_at = Some("2026-01-01T01:00:00Z".to_string());
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    b.assigned = Some("agent-2".to_string());
    b.started_at = Some("2026-01-01T02:00:00Z".to_string());
    b.completed_at = Some("2026-01-01T03:00:00Z".to_string());

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    evaluate_cycle_iteration(&mut graph, "b", &analysis);

    let a = graph.get_task("a").unwrap();
    assert!(a.assigned.is_none(), "assigned should be cleared on re-open");
    assert!(a.started_at.is_none(), "started_at should be cleared");
    assert!(a.completed_at.is_none(), "completed_at should be cleared");

    let b = graph.get_task("b").unwrap();
    assert!(b.assigned.is_none(), "assigned should be cleared on re-open");
    assert!(b.started_at.is_none(), "started_at should be cleared");
    assert!(b.completed_at.is_none(), "completed_at should be cleared");
}

#[test]
fn test_completion_adds_log_entry() {
    // Verify re-opening adds a log entry
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    evaluate_cycle_iteration(&mut graph, "b", &analysis);

    let a = graph.get_task("a").unwrap();
    assert!(
        !a.log.is_empty(),
        "Re-opened task should have a log entry"
    );
    assert!(
        a.log.last().unwrap().message.contains("Re-activated by cycle iteration"),
        "Log entry should mention cycle iteration"
    );
}

#[test]
fn test_completion_non_cycle_task_no_effect() {
    // Completing a non-cycle task should not trigger any cycle iteration
    let t = make_task_with_status("t", "Solo task", Status::Done);
    let mut graph = build_graph(vec![t]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "t", &analysis);
    assert!(reactivated.is_empty(), "Non-cycle task should not trigger iteration");
}

#[test]
fn test_completion_failed_member_prevents_iteration() {
    // If one member is Failed (not Done), cycle should not iterate
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Failed);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "a", &analysis);
    assert!(
        reactivated.is_empty(),
        "Failed member should prevent cycle iteration"
    );
}

#[test]
fn test_completion_three_node_cycle_iteration() {
    // Full 3-node cycle: A → B → C → A. All Done → iterate.
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    let mut c = make_task_with_status("c", "C", Status::Done);
    c.after = vec!["b".to_string()];

    let mut graph = build_graph(vec![a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "c", &analysis);

    assert_eq!(reactivated.len(), 3, "All 3 members should be re-activated");

    // All should be Open with iteration=1
    for id in &["a", "b", "c"] {
        let task = graph.get_task(id).unwrap();
        assert_eq!(task.status, Status::Open, "{} should be Open", id);
        assert_eq!(task.loop_iteration, 1, "{} iteration should be 1", id);
    }
}

#[test]
fn test_completion_multiple_independent_cycles() {
    // Two independent cycles. Completing all of cycle 1 should only re-open cycle 1.
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    // Second cycle: not all done
    let mut c = make_task_with_status("c", "C", Status::Done);
    c.after = vec!["d".to_string()];
    c.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut d = make_task("d", "D"); // Open
    d.after = vec!["c".to_string()];

    let mut graph = build_graph(vec![a, b, c, d]);
    let analysis = graph.compute_cycle_analysis();

    // Complete cycle 1 (trigger on b)
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    let reactivated_set: HashSet<&str> = reactivated.iter().map(|s| s.as_str()).collect();

    assert!(reactivated_set.contains("a"), "Cycle 1 should re-activate A");
    assert!(reactivated_set.contains("b"), "Cycle 1 should re-activate B");
    assert!(!reactivated_set.contains("c"), "Cycle 2 should not be affected");
    assert!(!reactivated_set.contains("d"), "Cycle 2 should not be affected");
}

#[test]
fn test_completion_iteration_less_than_guard() {
    // IterationLessThan(2) guard: iterate when current < 2, stop at 2.
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 10,
        guard: Some(LoopGuard::IterationLessThan(2)),
        delay: None,
    });
    a.loop_iteration = 1;
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    b.loop_iteration = 1;

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    // At iteration 1, which is < 2, so should iterate
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(!reactivated.is_empty(), "Should iterate (1 < 2)");
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 2);

    // Now at iteration 2, set Done again
    graph.get_task_mut("a").unwrap().status = Status::Done;
    graph.get_task_mut("b").unwrap().status = Status::Done;
    let analysis = graph.compute_cycle_analysis();

    // At iteration 2, which is NOT < 2, so should stop
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(reactivated.is_empty(), "Should NOT iterate (2 >= 2)");
}

// ===========================================================================
// 4. Migration tests
// ===========================================================================

#[test]
fn test_migrate_loops_command_noop() {
    // The migrate-loops command is now a noop since loops_to was removed
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("t1", "Task 1")]);

    let output = wg_ok(&wg_dir, &["migrate-loops"]);
    assert!(
        output.contains("No loops_to edges to migrate"),
        "Should report no migration needed. Output: {}",
        output
    );
}

#[test]
fn test_backward_compat_old_loops_to_loads() {
    // Old JSONL with loops_to field should still load correctly (silently ignored)
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    // Write a JSONL file with the old loops_to field
    let old_jsonl = r#"{"kind":"task","id":"t1","title":"Task 1","status":"open","after":[],"before":[],"requires":[],"tags":[],"skills":[],"inputs":[],"deliverables":[],"artifacts":[],"log":[],"retry_count":0,"loop_iteration":0,"paused":false,"visibility":"internal","loops_to":[{"target":"t1","max_iterations":3,"guard":null,"delay":null}]}
{"kind":"task","id":"t2","title":"Task 2","status":"open","after":["t1"],"before":[],"requires":[],"tags":[],"skills":[],"inputs":[],"deliverables":[],"artifacts":[],"log":[],"retry_count":0,"loop_iteration":0,"paused":false,"visibility":"internal"}"#;

    fs::write(wg_dir.join("graph.jsonl"), old_jsonl).unwrap();

    // Should load without error
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert!(graph.get_task("t1").is_some(), "Task t1 should load");
    assert!(graph.get_task("t2").is_some(), "Task t2 should load");
}

#[test]
fn test_backward_compat_old_loops_to_cli_works() {
    // Old JSONL with loops_to should work with CLI commands
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    let old_jsonl = r#"{"kind":"task","id":"t1","title":"Old task with loop","status":"open","after":[],"before":[],"requires":[],"tags":[],"skills":[],"inputs":[],"deliverables":[],"artifacts":[],"log":[],"retry_count":0,"loop_iteration":0,"paused":false,"visibility":"internal","loops_to":[{"target":"t1","max_iterations":5}]}"#;

    fs::write(wg_dir.join("graph.jsonl"), old_jsonl).unwrap();

    // wg list should work
    let output = wg_ok(&wg_dir, &["list"]);
    assert!(output.contains("t1"), "Should list the task");

    // wg show should work
    let output = wg_ok(&wg_dir, &["show", "t1"]);
    assert!(output.contains("Old task with loop"));
}

// ===========================================================================
// 5. CLI tests
// ===========================================================================

#[test]
fn test_cli_add_with_max_iterations() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(
        &wg_dir,
        &["add", "Cycle Header", "--id", "header", "--max-iterations", "5"],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("header").unwrap();
    assert!(task.cycle_config.is_some(), "Should have cycle_config");
    let config = task.cycle_config.as_ref().unwrap();
    assert_eq!(config.max_iterations, 5);
    assert!(config.guard.is_none());
    assert!(config.delay.is_none());
}

#[test]
fn test_cli_add_with_cycle_guard() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("sentinel", "Sentinel task")],
    );

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Guarded Cycle",
            "--id",
            "guarded",
            "--max-iterations",
            "5",
            "--cycle-guard",
            "task:sentinel=failed",
        ],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("guarded").unwrap();
    let config = task.cycle_config.as_ref().unwrap();
    assert_eq!(
        config.guard,
        Some(LoopGuard::TaskStatus {
            task: "sentinel".to_string(),
            status: Status::Failed,
        })
    );
}

#[test]
fn test_cli_add_with_cycle_delay() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Delayed Cycle",
            "--id",
            "delayed",
            "--max-iterations",
            "3",
            "--cycle-delay",
            "5m",
        ],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("delayed").unwrap();
    let config = task.cycle_config.as_ref().unwrap();
    assert_eq!(config.delay, Some("5m".to_string()));
}

#[test]
fn test_cli_add_cycle_guard_requires_max_iterations() {
    // --cycle-guard without --max-iterations should fail
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(
        &wg_dir,
        &["add", "Bad", "--id", "bad", "--cycle-guard", "always"],
    );
    assert!(
        !output.status.success(),
        "Should fail: --cycle-guard without --max-iterations"
    );
}

#[test]
fn test_cli_add_cycle_delay_requires_max_iterations() {
    // --cycle-delay without --max-iterations should fail
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(
        &wg_dir,
        &["add", "Bad", "--id", "bad", "--cycle-delay", "5m"],
    );
    assert!(
        !output.status.success(),
        "Should fail: --cycle-delay without --max-iterations"
    );
}

#[test]
fn test_cli_add_with_always_guard() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Always Loop",
            "--id",
            "always-loop",
            "--max-iterations",
            "10",
            "--cycle-guard",
            "always",
        ],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("always-loop").unwrap();
    let config = task.cycle_config.as_ref().unwrap();
    assert_eq!(config.guard, Some(LoopGuard::Always));
}

#[test]
fn test_cli_cycles_no_cycles() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![{
            let mut t2 = make_task("t2", "Task 2");
            t2.after = vec!["t1".to_string()];
            t2
        }, make_task("t1", "Task 1")],
    );

    let output = wg_ok(&wg_dir, &["cycles"]);
    assert!(
        output.contains("No cycles detected"),
        "Should report no cycles. Output: {}",
        output
    );
}

#[test]
fn test_cli_cycles_shows_cycle() {
    // Create a 2-node cycle
    let tmp = TempDir::new().unwrap();
    let mut a = make_task("a", "Task A");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    let output = wg_ok(&wg_dir, &["cycles"]);
    assert!(
        output.contains("Cycles detected"),
        "Should report cycles. Output: {}",
        output
    );
    assert!(
        output.contains("REDUCIBLE") || output.contains("IRREDUCIBLE"),
        "Should show reducibility. Output: {}",
        output
    );
}

#[test]
fn test_cli_cycles_json_output() {
    let tmp = TempDir::new().unwrap();
    let mut a = make_task("a", "Task A");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    let output = wg_ok(&wg_dir, &["cycles", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&output)
        .unwrap_or_else(|e| panic!("Invalid JSON: {}\nOutput: {}", e, output));

    assert!(parsed.get("cycle_count").is_some(), "JSON should have cycle_count");
    assert_eq!(
        parsed["cycle_count"].as_u64().unwrap(),
        1,
        "Should report 1 cycle"
    );
    assert!(parsed.get("cycles").is_some(), "JSON should have cycles array");
    assert!(parsed.get("back_edges").is_some(), "JSON should have back_edges");
}

#[test]
fn test_cli_check_reports_cycles() {
    // wg check should report cycles found in the graph
    let tmp = TempDir::new().unwrap();
    let mut a = make_task("a", "Task A");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    // Cycle warnings go to stderr; use --json for structured output on stdout
    let output = wg_ok(&wg_dir, &["check", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&output)
        .unwrap_or_else(|e| panic!("Invalid JSON: {}\nOutput: {}", e, output));

    // Structural cycles should be reported
    let structural = parsed.get("structural_cycles").unwrap().as_array().unwrap();
    assert!(
        !structural.is_empty(),
        "wg check --json should report structural_cycles. Output: {}",
        output
    );

    // Warnings should include cycles
    let warnings = parsed.get("warnings").unwrap().as_u64().unwrap();
    assert!(warnings > 0, "Should have warnings for cycles");
}

#[test]
fn test_cli_done_triggers_cycle_iteration() {
    // Set up a cycle where the last completion triggers re-opening.
    // Cycle: A ↔ B. A is header with cycle_config.
    // Pre-set A=Done. Then `wg done B` should trigger cycle iteration.
    let tmp = TempDir::new().unwrap();
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task("b", "B");
    b.status = Status::InProgress; // Was working on it
    b.after = vec!["a".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    // Complete B → both A and B should be re-opened
    let output = wg_ok(&wg_dir, &["done", "b"]);
    assert!(
        output.contains("re-activated"),
        "Should report cycle re-activation. Output: {}",
        output
    );

    // Verify both tasks are re-opened
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task_a = graph.get_task("a").unwrap();
    let task_b = graph.get_task("b").unwrap();
    assert_eq!(task_a.status, Status::Open, "A should be re-opened");
    assert_eq!(task_b.status, Status::Open, "B should be re-opened");
    assert_eq!(task_a.loop_iteration, 1, "A iteration should be 1");
    assert_eq!(task_b.loop_iteration, 1, "B iteration should be 1");
}

#[test]
fn test_cli_done_converged_stops_cycle() {
    // Completing with --converged should prevent re-opening.
    // The converged tag is added to the completing task. evaluate_cycle_iteration
    // checks the config owner for the converged tag. So the config owner must be
    // the task being completed with --converged.
    // Cycle: A ↔ B. B has cycle_config (is the config owner).
    // Pre-set A=Done. Then `wg done B --converged` → B gets converged tag.
    let tmp = TempDir::new().unwrap();
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "B (config owner)");
    b.status = Status::InProgress;
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
    });

    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    let output = wg_ok(&wg_dir, &["done", "b", "--converged"]);
    // Should NOT report re-activation
    assert!(
        !output.contains("re-activated"),
        "Should NOT re-activate when converged. Output: {}",
        output
    );

    // Both should remain Done
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task_a = graph.get_task("a").unwrap();
    let task_b = graph.get_task("b").unwrap();
    assert_eq!(task_a.status, Status::Done, "A should remain Done");
    assert_eq!(task_b.status, Status::Done, "B should remain Done");
}

#[test]
fn test_cli_done_cycle_three_nodes() {
    // 3-node cycle via CLI: A → B → C → A
    // Pre-set A=Done, B=Done. Then `wg done C` triggers iteration.
    let tmp = TempDir::new().unwrap();
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "C");
    c.status = Status::InProgress;
    c.after = vec!["b".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![a, b, c]);

    let output = wg_ok(&wg_dir, &["done", "c"]);
    assert!(
        output.contains("re-activated"),
        "Should re-activate all cycle members. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    for id in &["a", "b", "c"] {
        let task = graph.get_task(id).unwrap();
        assert_eq!(task.status, Status::Open, "{} should be Open", id);
        assert_eq!(task.loop_iteration, 1, "{} iteration should be 1", id);
    }
}

#[test]
fn test_cli_done_max_iterations_stops_cli() {
    // max_iterations = 1, already at iteration 1 → should not re-open.
    let tmp = TempDir::new().unwrap();
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 1,
        guard: None,
        delay: None,
    });
    a.loop_iteration = 1;
    let mut b = make_task("b", "B");
    b.status = Status::InProgress;
    b.after = vec!["a".to_string()];
    b.loop_iteration = 1;

    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    let output = wg_ok(&wg_dir, &["done", "b"]);
    assert!(
        !output.contains("re-activated"),
        "Should NOT re-activate at max iterations. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
}

// ===========================================================================
// 6. Edge cases
// ===========================================================================

#[test]
fn test_convergence_on_non_cycle_task() {
    // --converged on a non-cycle task should just add the tag, no error
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task_with_status("solo", "Solo task", Status::InProgress)],
    );

    let output = wg_ok(&wg_dir, &["done", "solo", "--converged"]);
    assert!(output.contains("done"), "Should succeed");

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("solo").unwrap();
    assert!(
        task.tags.contains(&"converged".to_string()),
        "Should have converged tag"
    );
}

#[test]
fn test_cycle_with_mixed_statuses_no_iteration() {
    // Cycle with mixed statuses (InProgress, Open) should not trigger iteration
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task_with_status("b", "B", Status::InProgress);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "a", &analysis);
    assert!(reactivated.is_empty(), "Should not iterate with InProgress member");
}

#[test]
fn test_cycle_header_removed_breaks_cycle() {
    // If the header is removed, the cycle should disappear
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);

    // Verify cycle exists
    let analysis = graph.compute_cycle_analysis();
    assert_eq!(analysis.cycles.len(), 1);

    // Remove a node from the cycle
    graph.remove_node("a");

    // Cycle should disappear
    let analysis = graph.compute_cycle_analysis();
    assert!(analysis.cycles.is_empty(), "Cycle should disappear after removing member");
}

#[test]
fn test_add_task_creates_cycle() {
    // Adding a task that creates a cycle should be detectable
    let t1 = make_task("t1", "Task 1");
    let mut t2 = make_task("t2", "Task 2");
    t2.after = vec!["t1".to_string()];

    let mut graph = build_graph(vec![t1, t2]);

    // No cycle yet
    let analysis = graph.compute_cycle_analysis();
    assert!(analysis.cycles.is_empty());

    // Add back-edge: t1 after t2 → creates cycle
    let mut t1_mut = graph.get_task("t1").unwrap().clone();
    t1_mut.after = vec!["t2".to_string()];
    graph.add_node(Node::Task(t1_mut));

    // Now there's a cycle
    let analysis = graph.compute_cycle_analysis();
    assert_eq!(analysis.cycles.len(), 1, "Should detect the new cycle");
}

#[test]
fn test_normal_ready_tasks_no_cycle_exemption() {
    // Regular ready_tasks() (non-cycle-aware) should NOT exempt back-edges
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
    });
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];

    let graph = build_graph(vec![a, b]);

    let ready = ready_tasks(&graph);
    assert!(
        ready.is_empty(),
        "Non-cycle-aware ready_tasks should not exempt back-edges"
    );
}
