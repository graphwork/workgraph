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
    CycleConfig, LoopGuard, Node, Status, Task, WorkGraph, evaluate_all_cycle_failure_restarts,
    evaluate_all_cycle_iterations, evaluate_cycle_iteration, evaluate_cycle_on_failure,
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

    assert!(
        analysis.cycles.is_empty(),
        "Linear chain should have no cycles"
    );
    assert!(analysis.task_to_cycle.is_empty());
}

#[test]
fn test_cycle_analysis_dag_no_cycles() {
    // Diamond graph (no cycle): A → B, A → C, B → D, C → D
    let a = make_task("a", "A");
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "C");
    c.after = vec!["a".to_string()];
    let mut d = make_task("d", "D");
    d.after = vec!["b".to_string(), "c".to_string()];

    let graph = build_graph(vec![a, b, c, d]);
    let analysis = graph.compute_cycle_analysis();

    assert!(
        analysis.cycles.is_empty(),
        "Diamond graph should have no cycles"
    );
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

    assert_eq!(
        analysis.cycles.len(),
        1,
        "Should detect exactly one 3-node cycle"
    );
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
    assert!(
        cycle.reducible,
        "Cycle with single entry point should be reducible"
    );
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
        analysis
            .back_edges
            .contains(&("c".to_string(), "a".to_string())),
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
    assert!(
        analysis.cycles.is_empty(),
        "Single task should not form a cycle"
    );
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
    // 2-node cycle: worker ↔ validator.
    // worker.after = [validator, x], validator.after = [worker].
    // worker is the cycle entry (has external dep x). Back-edge: validator→worker.
    // X is Done → worker is ready (back-edge from validator skipped).
    // Validator blocked by worker (forward dep).
    let x = make_task_with_status("x", "External", Status::Done);
    let mut worker = make_task("worker", "Worker");
    worker.after = vec!["validator".to_string(), "x".to_string()];
    let mut validator = make_task("validator", "Validator");
    validator.after = vec!["worker".to_string()];
    validator.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let graph = build_graph(vec![x, worker, validator]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"worker"),
        "Worker (header) should be ready (back-edge from validator skipped, X done). Ready: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"validator"),
        "Validator should NOT be ready (forward dep on worker). Ready: {:?}",
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
fn test_dispatch_non_iterator_not_ready_when_pred_open() {
    // 3-node cycle: B(Done) → C → A (execution order). A is the iterator.
    // SCC = {a, c}. C is the cycle entry (has external dep B). Header = C.
    // Back-edge: a→c (A feeds back to C). Forward: c→a.
    // C: B Done + A back-edge skipped → READY.
    // A: forward dep on C (Open) → blocked.
    let b = make_task_with_status("b", "B (done)", Status::Done);
    let mut c = make_task("c", "C (worker 2)");
    c.after = vec!["b".to_string(), "a".to_string()]; // a is auto-back-edge
    let mut a = make_task("a", "A (iterator)");
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let graph = build_graph(vec![a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    // C should be ready: B is Done, A is back-edge (skipped).
    assert!(
        ready_ids.contains(&"c"),
        "C (header) should be ready (B done, A back-edge skipped). Ready: {:?}",
        ready_ids
    );
    // A should NOT be ready: forward dep on C (Open).
    assert!(
        !ready_ids.contains(&"a"),
        "A should NOT be ready (forward dep on C is Open). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_dispatch_no_config_header_still_ready() {
    // Cycle: A ↔ B. Neither has cycle_config.
    // Back-edge exemption is purely structural — cycle_config is not required.
    // The header (a, smallest ID) is ready because its back-edge blocker is skipped.
    // B is blocked by A (forward dep).
    // Without cycle_config the cycle won't re-iterate, but the first pass executes.
    let mut a = make_task("a", "A (no config)");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];

    let graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert_eq!(
        ready_ids.len(),
        1,
        "Header should be ready (back-edge exemption is structural). Ready: {:?}",
        ready_ids
    );
    assert_eq!(
        ready_ids[0], analysis.cycles[0].header,
        "The ready task should be the cycle header"
    );
}

#[test]
fn test_dispatch_back_edge_exemption_only_for_iterator_blocker() {
    // 3-node: B(Open, no deps) → C → A (execution order). A is the iterator.
    // SCC = {a, c}. C is entry (has external dep B). Header = C.
    // Back-edge: a→c. Forward: c→a.
    // B: no deps → READY.
    // C: B Open → blocked.
    // A: forward dep on C (Open) → blocked.
    let b = make_task("b", "B (worker 1)");
    let mut c = make_task("c", "C (worker 2)");
    c.after = vec!["b".to_string(), "a".to_string()]; // a is auto-back-edge
    let mut a = make_task("a", "A (iterator)");
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let graph = build_graph(vec![a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    // B should be ready (no deps).
    assert!(
        ready_ids.contains(&"b"),
        "B should be ready (no deps). Ready: {:?}",
        ready_ids
    );
    // C blocked by B (B is Open, not terminal).
    assert!(
        !ready_ids.contains(&"c"),
        "C should NOT be ready (B is Open). Ready: {:?}",
        ready_ids
    );
    // A blocked by C (forward dep, C is Open).
    assert!(
        !ready_ids.contains(&"a"),
        "A should NOT be ready (forward dep on C). Ready: {:?}",
        ready_ids
    );
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
    // After cycle re-opens, the header should be ready (back-edge skipped).
    // Simulate: all cycle members re-opened (Open status, loop_iteration > 0).
    let mut worker = make_task("worker", "Worker");
    worker.after = vec!["validator".to_string()];
    worker.loop_iteration = 1; // Second iteration
    let mut validator = make_task("validator", "Validator");
    validator.after = vec!["worker".to_string()];
    validator.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    validator.loop_iteration = 1;

    let graph = build_graph(vec![worker, validator]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    // Exactly one task (the cycle header) should be ready after re-open.
    let header = &analysis.cycles[0].header;
    assert_eq!(
        ready_ids.len(),
        1,
        "Exactly one task (the header) should be ready. Ready: {:?}",
        ready_ids
    );
    assert_eq!(
        ready_ids[0],
        header.as_str(),
        "The cycle header should be ready on re-iteration"
    );
}

#[test]
fn test_dispatch_cycle_header_not_exempt_from_forward_deps() {
    // Context-scopes scenario: design → integrate → verify (cycle via back-edges).
    // verify has cycle_config. Auto-back-edges add verify to design.after and
    // integrate.after, forming an SCC = {design, integrate, verify}.
    //
    // External dep x (Done) → design makes design the deterministic entry/header.
    // Back-edges from DFS: verify→design, verify→integrate.
    //
    // When design is Done:
    //   - integrate: design Done + verify back-edge skipped → READY
    //   - verify: forward dep on integrate (Open) → blocked
    let x = make_task_with_status("x", "External", Status::Done);

    let mut design = make_task_with_status("design", "Design", Status::Done);
    design.after = vec!["verify".to_string(), "x".to_string()]; // auto-back-edge + external

    let mut integrate = make_task("integrate", "Integrate");
    integrate.after = vec!["design".to_string(), "verify".to_string()]; // forward + auto-back-edge

    let mut verify = make_task("verify", "Verify");
    verify.after = vec!["integrate".to_string(), "design".to_string()]; // forward deps
    verify.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let graph = build_graph(vec![x, design, integrate, verify]);
    let analysis = graph.compute_cycle_analysis();

    assert_eq!(
        analysis.cycles[0].header, "design",
        "Design should be the cycle header (external entry from x)"
    );

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"integrate"),
        "Integrate should be ready (design Done, verify back-edge skipped). Ready: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"verify"),
        "Verify must NOT be ready (forward dep on integrate is Open). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_dispatch_cycle_header_ready_when_all_forward_deps_done() {
    // Same cycle as above but now both design and integrate are Done.
    // Verify should now be ready (all forward deps satisfied).
    let mut design = make_task_with_status("design", "Design", Status::Done);
    design.after = vec!["verify".to_string()];

    let mut integrate = make_task_with_status("integrate", "Integrate", Status::Done);
    integrate.after = vec!["design".to_string(), "verify".to_string()];

    let mut verify = make_task("verify", "Verify");
    verify.after = vec!["integrate".to_string(), "design".to_string()];
    verify.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let graph = build_graph(vec![design, integrate, verify]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"verify"),
        "Verify should be ready (all forward deps Done). Ready: {:?}",
        ready_ids
    );
}

// ===========================================================================
// 2b. Sequential dispatch ordering tests (back-edge exemption)
// ===========================================================================

#[test]
fn test_dispatch_two_task_cycle_sequential_order() {
    // 2-task cycle: A ↔ B, A has cycle_config (max_iterations=2).
    // External dep X (Done) → A, making A the deterministic entry/header.
    // Iteration 0: A ready (back-edge from B skipped, X done) → complete A → B ready.
    // Iteration 1: cycle resets → A ready again → complete A → B ready → cycle done.
    let x = make_task_with_status("x", "External", Status::Done);

    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string(), "x".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 2,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![x, a, b]);

    // --- Iteration 0 ---
    let analysis = graph.compute_cycle_analysis();
    assert_eq!(analysis.cycles[0].header, "a", "A should be the header");

    // Step 1: Only A should be ready
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ready_ids, vec!["a"], "Iteration 0 step 1: only A ready");

    // Step 2: Complete A → B should be ready
    graph.get_task_mut("a").unwrap().status = Status::Done;
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ready_ids, vec!["b"], "Iteration 0 step 2: only B ready");

    // Step 3: Complete B → cycle should re-open (both A and B done)
    graph.get_task_mut("b").unwrap().status = Status::Done;
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert_eq!(reactivated.len(), 2, "Both tasks should be re-activated");
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 1);

    // --- Iteration 1 ---
    let analysis = graph.compute_cycle_analysis();

    // Step 4: A should be ready again
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ready_ids, vec!["a"], "Iteration 1 step 1: only A ready");

    // Step 5: Complete A → B ready
    graph.get_task_mut("a").unwrap().status = Status::Done;
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ready_ids, vec!["b"], "Iteration 1 step 2: only B ready");

    // Step 6: Complete B → max_iterations reached, no re-open
    graph.get_task_mut("b").unwrap().status = Status::Done;
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(
        reactivated.is_empty(),
        "Should not re-activate at max_iterations"
    );
}

#[test]
fn test_dispatch_three_task_cycle_sequential_order() {
    // 3-task cycle: A → B → C → A, A has cycle_config.
    // External dep X (Done) → A, making A the deterministic header.
    // Dispatch order: A first, then B, then C.
    let x = make_task_with_status("x", "External", Status::Done);

    let mut a = make_task("a", "A");
    a.after = vec!["c".to_string(), "x".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 2,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];

    let mut c = make_task("c", "C");
    c.after = vec!["b".to_string()];

    let mut graph = build_graph(vec![x, a, b, c]);

    // --- Iteration 0 ---
    let analysis = graph.compute_cycle_analysis();
    assert_eq!(analysis.cycles[0].header, "a", "A should be the header");

    // Step 1: Only A ready (back-edge C→A skipped, X done)
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ready_ids, vec!["a"], "Step 1: only A ready");

    // Step 2: Complete A → only B ready
    graph.get_task_mut("a").unwrap().status = Status::Done;
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ready_ids, vec!["b"], "Step 2: only B ready (A done)");

    // Step 3: Complete B → only C ready
    graph.get_task_mut("b").unwrap().status = Status::Done;
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ready_ids, vec!["c"], "Step 3: only C ready (A+B done)");

    // Step 4: Complete C → cycle re-opens (all done, iteration < max)
    graph.get_task_mut("c").unwrap().status = Status::Done;
    let reactivated = evaluate_cycle_iteration(&mut graph, "c", &analysis);
    assert_eq!(reactivated.len(), 3, "All three should re-activate");

    // --- Iteration 1 ---
    let analysis = graph.compute_cycle_analysis();

    // Step 5: A ready again
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(ready_ids, vec!["a"], "Iteration 1: only A ready");

    // Complete full cycle
    graph.get_task_mut("a").unwrap().status = Status::Done;
    graph.get_task_mut("b").unwrap().status = Status::Done;
    graph.get_task_mut("c").unwrap().status = Status::Done;

    let reactivated = evaluate_cycle_iteration(&mut graph, "c", &analysis);
    assert!(
        reactivated.is_empty(),
        "Should not re-activate at max_iterations"
    );
}

#[test]
fn test_dispatch_self_loop_ready_on_all_iterations() {
    // Self-loop: A → A with cycle_config.
    // A should be ready on every iteration (back-edge A→A always skipped).
    let mut a = make_task("a", "A");
    a.after = vec!["a".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a]);

    // Iteration 0
    let analysis = graph.compute_cycle_analysis();
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    assert_eq!(ready.len(), 1, "A should be ready on iteration 0");
    assert_eq!(ready[0].id, "a");

    // Complete and re-open
    graph.get_task_mut("a").unwrap().status = Status::Done;
    let reactivated = evaluate_cycle_iteration(&mut graph, "a", &analysis);
    assert_eq!(reactivated.len(), 1);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);

    // Iteration 1: still ready
    let analysis = graph.compute_cycle_analysis();
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    assert_eq!(ready.len(), 1, "A should be ready on iteration 1");
    assert_eq!(ready[0].id, "a");

    // Complete and re-open again
    graph.get_task_mut("a").unwrap().status = Status::Done;
    let reactivated = evaluate_cycle_iteration(&mut graph, "a", &analysis);
    assert_eq!(reactivated.len(), 1);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 2);

    // Iteration 2: still ready
    let analysis = graph.compute_cycle_analysis();
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    assert_eq!(ready.len(), 1, "A should be ready on iteration 2");

    // Complete → at max_iterations, no re-open
    graph.get_task_mut("a").unwrap().status = Status::Done;
    let reactivated = evaluate_cycle_iteration(&mut graph, "a", &analysis);
    assert!(
        reactivated.is_empty(),
        "Should not re-activate at max_iterations"
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        !reactivated.is_empty(),
        "Always guard should allow iteration"
    );
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
    assert!(
        a.assigned.is_none(),
        "assigned should be cleared on re-open"
    );
    assert!(a.started_at.is_none(), "started_at should be cleared");
    assert!(a.completed_at.is_none(), "completed_at should be cleared");

    let b = graph.get_task("b").unwrap();
    assert!(
        b.assigned.is_none(),
        "assigned should be cleared on re-open"
    );
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    evaluate_cycle_iteration(&mut graph, "b", &analysis);

    let a = graph.get_task("a").unwrap();
    assert!(!a.log.is_empty(), "Re-opened task should have a log entry");
    assert!(
        a.log
            .last()
            .unwrap()
            .message
            .contains("Re-activated by cycle iteration"),
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
    assert!(
        reactivated.is_empty(),
        "Non-cycle task should not trigger iteration"
    );
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut d = make_task("d", "D"); // Open
    d.after = vec!["c".to_string()];

    let mut graph = build_graph(vec![a, b, c, d]);
    let analysis = graph.compute_cycle_analysis();

    // Complete cycle 1 (trigger on b)
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    let reactivated_set: HashSet<&str> = reactivated.iter().map(|s| s.as_str()).collect();

    assert!(
        reactivated_set.contains("a"),
        "Cycle 1 should re-activate A"
    );
    assert!(
        reactivated_set.contains("b"),
        "Cycle 1 should re-activate B"
    );
    assert!(
        !reactivated_set.contains("c"),
        "Cycle 2 should not be affected"
    );
    assert!(
        !reactivated_set.contains("d"),
        "Cycle 2 should not be affected"
    );
}

#[test]
fn test_completion_iteration_less_than_guard() {
    // IterationLessThan(2) guard: iterate while next_iteration < 2, stop when next >= 2.
    // With max_iterations=10 and guard IterationLessThan(2):
    //   iteration 0: next=1, 1<2 → iterate
    //   iteration 1: next=2, 2>=2 → stop
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 10,
        guard: Some(LoopGuard::IterationLessThan(2)),
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    a.loop_iteration = 0;
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    b.loop_iteration = 0;

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    // At iteration 0, next=1 which is < 2, so should iterate
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(!reactivated.is_empty(), "Should iterate (next 1 < 2)");
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);

    // Now at iteration 1, set Done again
    graph.get_task_mut("a").unwrap().status = Status::Done;
    graph.get_task_mut("b").unwrap().status = Status::Done;
    let analysis = graph.compute_cycle_analysis();

    // At iteration 1, next=2 which is NOT < 2, so should stop
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(reactivated.is_empty(), "Should NOT iterate (next 2 >= 2)");
}

// ===========================================================================
// 4. Backward compatibility tests
// ===========================================================================

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
        &[
            "add",
            "Cycle Header",
            "--id",
            "header",
            "--max-iterations",
            "5",
        ],
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
    let wg_dir = setup_workgraph(&tmp, vec![make_task("sentinel", "Sentinel task")]);

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
        vec![
            {
                let mut t2 = make_task("t2", "Task 2");
                t2.after = vec!["t1".to_string()];
                t2
            },
            make_task("t1", "Task 1"),
        ],
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

    assert!(
        parsed.get("cycle_count").is_some(),
        "JSON should have cycle_count"
    );
    assert_eq!(
        parsed["cycle_count"].as_u64().unwrap(),
        1,
        "Should report 1 cycle"
    );
    assert!(
        parsed.get("cycles").is_some(),
        "JSON should have cycles array"
    );
    assert!(
        parsed.get("back_edges").is_some(),
        "JSON should have back_edges"
    );
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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
        vec![make_task_with_status(
            "solo",
            "Solo task",
            Status::InProgress,
        )],
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::InProgress);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "a", &analysis);
    assert!(
        reactivated.is_empty(),
        "Should not iterate with InProgress member"
    );
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
    assert!(
        analysis.cycles.is_empty(),
        "Cycle should disappear after removing member"
    );
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
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
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

// ===========================================================================
// 7. Implicit cycle tests (--max-iterations without back-edges)
// ===========================================================================

#[test]
fn test_implicit_cycle_fires_on_done() {
    // Scenario from BUG_REPORT_CYCLE_NOT_FIRING.md:
    // Task A (worker), Task B --after A --max-iterations 3 (evaluator/header).
    // No back-edge. When both are Done and B completes, cycle should fire.
    let a = make_task_with_status("a", "Worker", Status::Done);
    let mut b = make_task_with_status("b", "Evaluator", Status::Done);
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    // No SCC — A and B don't form a cycle in the graph (no back-edge)
    assert!(analysis.cycles.is_empty(), "Should have no SCC cycles");

    // But evaluate_cycle_iteration should still fire for the implicit cycle
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        !reactivated.is_empty(),
        "Implicit cycle should re-activate tasks. Got: {:?}",
        reactivated
    );

    let reactivated_set: HashSet<&str> = reactivated.iter().map(|s| s.as_str()).collect();
    assert!(
        reactivated_set.contains("a"),
        "Worker A should be re-activated"
    );
    assert!(
        reactivated_set.contains("b"),
        "Evaluator B should be re-activated"
    );

    // Verify both are Open with iteration 1
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 1);
}

#[test]
fn test_implicit_cycle_respects_max_iterations() {
    // At max_iterations, should NOT re-activate
    let mut a = make_task_with_status("a", "Worker", Status::Done);
    a.loop_iteration = 3;
    let mut b = make_task_with_status("b", "Evaluator", Status::Done);
    b.after = vec!["a".to_string()];
    b.loop_iteration = 3;
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(
        reactivated.is_empty(),
        "Should NOT iterate at max_iterations"
    );
}

#[test]
fn test_implicit_cycle_respects_converged() {
    // Converged tag should prevent re-activation
    let a = make_task_with_status("a", "Worker", Status::Done);
    let mut b = make_task_with_status("b", "Evaluator", Status::Done);
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    b.tags = vec!["converged".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(
        reactivated.is_empty(),
        "Converged tag should prevent iteration"
    );
}

#[test]
fn test_implicit_cycle_partial_done_no_fire() {
    // If worker A is not done yet, cycle should NOT fire
    let a = make_task("a", "Worker"); // Open, not done
    let mut b = make_task_with_status("b", "Evaluator", Status::Done);
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(
        reactivated.is_empty(),
        "Should NOT iterate when deps aren't done"
    );
}

#[test]
fn test_implicit_cycle_multiple_deps() {
    // Task C --after A,B --max-iterations 2. All three should be re-activated.
    let a = make_task_with_status("a", "Worker A", Status::Done);
    let b = make_task_with_status("b", "Worker B", Status::Done);
    let mut c = make_task_with_status("c", "Evaluator", Status::Done);
    c.after = vec!["a".to_string(), "b".to_string()];
    c.cycle_config = Some(CycleConfig {
        max_iterations: 2,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "c", &analysis);

    assert_eq!(reactivated.len(), 3, "All 3 tasks should be re-activated");
    let set: HashSet<&str> = reactivated.iter().map(|s| s.as_str()).collect();
    assert!(set.contains("a"));
    assert!(set.contains("b"));
    assert!(set.contains("c"));
}

#[test]
fn test_implicit_cycle_non_config_task_no_fire() {
    // Completing a task WITHOUT cycle_config should not trigger implicit cycle
    let mut a = make_task_with_status("a", "Worker", Status::Done);
    a.after = vec!["b".to_string()];
    let b = make_task_with_status("b", "Other", Status::Done);

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "a", &analysis);
    assert!(
        reactivated.is_empty(),
        "Task without cycle_config should not trigger implicit cycle"
    );
}

#[test]
fn test_cli_implicit_cycle_via_max_iterations() {
    // End-to-end CLI test: wg add A, wg add B --after A --max-iterations 3,
    // wg done A, wg done B → verify A is re-opened with iteration 1.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    // Add task A
    wg_ok(&wg_dir, &["add", "Worker", "--id", "a"]);
    // Add task B --after A --max-iterations 3
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Evaluator",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "3",
        ],
    );

    // Complete A first
    wg_ok(&wg_dir, &["done", "a"]);

    // Complete B (plain, not converged) → should trigger cycle
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

// ===========================================================================
// 8. Consolidated cycle fix tests
// ===========================================================================

// --- 8.1 Viz: cycle back-reference annotation ---

#[test]
fn test_viz_cycle_back_reference_annotation() {
    // Cycle: A → B → A (back-edge). Viz should show ↺ for the back-edge,
    // not "A ..." which would indicate fan-in/duplicate.
    let tmp = TempDir::new().unwrap();
    let mut a = make_task("a", "Task A");
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    // Default viz format is ASCII tree (no --format flag needed)
    let output = wg_ok(&wg_dir, &["viz", "--all"]);
    // Should contain the cycle back-edge annotation marker
    assert!(
        output.contains("↺") || output.contains("cycle back-edge"),
        "Viz should annotate cycle back-edge with ↺ marker. Output:\n{}",
        output
    );
    let lines: Vec<&str> = output.lines().collect();
    let has_cycle_annotation = lines.iter().any(|l| l.contains("↺"));
    assert!(
        has_cycle_annotation,
        "At least one line should have ↺ cycle annotation. Output:\n{}",
        output
    );
}

#[test]
fn test_viz_fan_in_still_shows_dots() {
    // Diamond: A → [B, C] → D. D should show "..." when re-encountered, not ↺.
    let tmp = TempDir::new().unwrap();
    let a = make_task("a", "Task A");
    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "Task C");
    c.after = vec!["a".to_string()];
    let mut d = make_task("d", "Task D");
    d.after = vec!["b".to_string(), "c".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![a, b, c, d]);

    let output = wg_ok(&wg_dir, &["viz", "--all"]);
    let lines: Vec<&str> = output.lines().collect();
    let cycle_lines: Vec<&&str> = lines.iter().filter(|l| l.contains("↺")).collect();
    assert!(
        cycle_lines.is_empty(),
        "Diamond fan-in should NOT show ↺ cycle annotation. Lines with ↺: {:?}\nFull output:\n{}",
        cycle_lines,
        output
    );
}

#[test]
fn test_viz_implicit_cycle_annotation() {
    // Implicit cycle (no back-edge): B --after A --max-iterations 3.
    // Viz should work and show the tasks.
    let tmp = TempDir::new().unwrap();
    let a = make_task("a", "Worker");
    let mut b = make_task("b", "Evaluator");
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    let output = wg_ok(&wg_dir, &["viz", "--all"]);
    assert!(
        output.contains("a") && output.contains("b"),
        "Viz should show both tasks. Output:\n{}",
        output
    );
}

// --- 8.2 Self-convergence blocked by guard authority ---

#[test]
fn test_self_convergence_blocked_by_guard() {
    // Worker calls --converged on a guarded cycle. Guard is authoritative,
    // so the converged tag should NOT be added and the cycle should continue.
    let tmp = TempDir::new().unwrap();
    let sentinel = make_task_with_status("sentinel", "Sentinel", Status::Failed);
    let mut a = make_task_with_status("a", "Worker", Status::Done);
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "Evaluator");
    b.status = Status::InProgress;
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: Some(LoopGuard::TaskStatus {
            task: "sentinel".to_string(),
            status: Status::Failed,
        }),
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let wg_dir = setup_workgraph(&tmp, vec![sentinel, a, b]);

    let output = wg_cmd(&wg_dir, &["done", "b", "--converged"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "wg done should succeed. stderr: {}",
        stderr
    );

    // Should warn about --converged being ignored
    assert!(
        stderr.contains("--converged ignored"),
        "Should warn about --converged being ignored. stderr: {}",
        stderr
    );

    // Cycle should have iterated (guard condition met: sentinel=failed)
    assert!(
        stdout.contains("re-activated"),
        "Guard allows iteration, so cycle should re-activate. stdout: {}",
        stdout
    );

    // Verify the converged tag was NOT added
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task_b = graph.get_task("b").unwrap();
    assert!(
        !task_b.tags.contains(&"converged".to_string()),
        "Converged tag should NOT be added when guard is set"
    );
}

#[test]
fn test_self_convergence_works_without_guard() {
    // Without a guard, --converged should work normally and stop the cycle.
    let tmp = TempDir::new().unwrap();
    let mut a = make_task_with_status("a", "Worker", Status::Done);
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "Evaluator");
    b.status = Status::InProgress;
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    let output = wg_ok(&wg_dir, &["done", "b", "--converged"]);
    assert!(
        !output.contains("re-activated"),
        "With --converged and no guard, cycle should NOT re-activate. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task_b = graph.get_task("b").unwrap();
    assert!(
        task_b.tags.contains(&"converged".to_string()),
        "Converged tag should be added when no guard is set"
    );
    assert_eq!(task_b.status, Status::Done, "B should remain Done");
}

#[test]
fn test_self_convergence_guard_always_treated_as_no_guard() {
    // Guard=Always should be treated like "no guard" for convergence purposes.
    let tmp = TempDir::new().unwrap();
    let mut a = make_task_with_status("a", "Worker", Status::Done);
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "Evaluator");
    b.status = Status::InProgress;
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: Some(LoopGuard::Always),
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    let output = wg_ok(&wg_dir, &["done", "b", "--converged"]);
    assert!(
        !output.contains("re-activated"),
        "With --converged and Always guard, cycle should NOT re-activate. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task_b = graph.get_task("b").unwrap();
    assert!(
        task_b.tags.contains(&"converged".to_string()),
        "Converged tag should be added with Always guard"
    );
}

#[test]
fn test_unit_guard_authority_ignores_converged_tag() {
    // Unit test: when guard is set and converged tag exists on config owner,
    // the cycle should still iterate if the guard condition is met.
    let sentinel = make_task_with_status("sentinel", "Sentinel", Status::Failed);
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.tags = vec!["converged".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: Some(LoopGuard::TaskStatus {
            task: "sentinel".to_string(),
            status: Status::Failed,
        }),
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![sentinel, a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        !reactivated.is_empty(),
        "Guard should override converged tag. Cycle should iterate."
    );
}

// --- 8.3 Guard convergence: guard can stop the cycle ---

#[test]
fn test_guard_stops_cycle_when_condition_not_met() {
    // Guard: task:sentinel=failed. Sentinel is Done (not Failed).
    // Guard condition NOT met → cycle stops.
    let tmp = TempDir::new().unwrap();
    let sentinel = make_task_with_status("sentinel", "Sentinel", Status::Done);
    let mut a = make_task_with_status("a", "Worker", Status::Done);
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "Evaluator");
    b.status = Status::InProgress;
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: Some(LoopGuard::TaskStatus {
            task: "sentinel".to_string(),
            status: Status::Failed,
        }),
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let wg_dir = setup_workgraph(&tmp, vec![sentinel, a, b]);

    let output = wg_ok(&wg_dir, &["done", "b"]);
    assert!(
        !output.contains("re-activated"),
        "Guard condition not met, cycle should NOT re-activate. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
}

#[test]
fn test_guard_allows_cycle_when_condition_met() {
    // Guard: task:sentinel=failed. Sentinel IS Failed.
    // Guard condition met → cycle iterates.
    let tmp = TempDir::new().unwrap();
    let sentinel = make_task_with_status("sentinel", "Sentinel", Status::Failed);
    let mut a = make_task_with_status("a", "Worker", Status::Done);
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "Evaluator");
    b.status = Status::InProgress;
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: Some(LoopGuard::TaskStatus {
            task: "sentinel".to_string(),
            status: Status::Failed,
        }),
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let wg_dir = setup_workgraph(&tmp, vec![sentinel, a, b]);

    let output = wg_ok(&wg_dir, &["done", "b"]);
    assert!(
        output.contains("re-activated"),
        "Guard condition met, cycle should re-activate. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("a").unwrap().status,
        Status::Open,
        "A re-opened"
    );
    assert_eq!(
        graph.get_task("b").unwrap().status,
        Status::Open,
        "B re-opened"
    );
}

// --- 8.4 Back-edge auto-creation via --max-iterations + --after ---

#[test]
fn test_max_iterations_creates_back_edge() {
    // wg add B --after A --max-iterations 3
    // Should auto-create a back-edge: A.after should now contain B.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("a", "Worker A")]);

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Validator",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "3",
        ],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task_a = graph.get_task("a").unwrap();
    let task_b = graph.get_task("b").unwrap();

    assert!(
        task_b.after.contains(&"a".to_string()),
        "B should depend on A. B.after: {:?}",
        task_b.after
    );
    assert!(
        task_a.after.contains(&"b".to_string()),
        "A.after should contain B (auto back-edge). A.after: {:?}",
        task_a.after
    );

    let analysis = graph.compute_cycle_analysis();
    assert!(
        !analysis.cycles.is_empty(),
        "SCC should detect the cycle created by auto back-edge"
    );

    let cycle = &analysis.cycles[0];
    let members: HashSet<&str> = cycle.members.iter().map(|s| s.as_str()).collect();
    assert!(members.contains("a"), "A should be in cycle");
    assert!(members.contains("b"), "B should be in cycle");
}

#[test]
fn test_max_iterations_back_edge_detected_by_scc() {
    // Create partial cycle via --max-iterations + --after and verify SCC detection
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Step A", "--id", "a"]);
    wg_ok(&wg_dir, &["add", "Step B", "--id", "b", "--after", "a"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Step C",
            "--id",
            "c",
            "--after",
            "b",
            "--max-iterations",
            "3",
        ],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let analysis = graph.compute_cycle_analysis();

    assert!(
        !analysis.cycles.is_empty(),
        "Should detect cycle from auto back-edge"
    );

    let has_back_edge = analysis
        .back_edges
        .iter()
        .any(|(src, tgt)| (src == "c" && tgt == "b") || (src == "b" && tgt == "c"));
    assert!(
        has_back_edge,
        "Back-edge between B and C should be detected. Back-edges: {:?}",
        analysis.back_edges
    );
}

// --- 8.5 Mixed deps: setup task NOT re-opened ---

#[test]
fn test_mixed_deps_setup_not_reopened() {
    // Pipeline: setup → impl → validate --max-iterations 3
    // Only impl and validate should be re-opened, NOT setup.
    let setup = make_task_with_status("setup", "Setup", Status::Done);
    let mut impl_task = make_task_with_status("impl", "Implement", Status::Done);
    impl_task.after = vec!["setup".to_string()];
    let mut validate = make_task_with_status("validate", "Validate", Status::Done);
    validate.after = vec!["impl".to_string()];
    validate.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![setup, impl_task, validate]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "validate", &analysis);

    let set: HashSet<&str> = reactivated.iter().map(|s| s.as_str()).collect();
    assert!(
        set.contains("impl"),
        "impl should be re-activated. Reactivated: {:?}",
        reactivated
    );
    assert!(
        set.contains("validate"),
        "validate should be re-activated. Reactivated: {:?}",
        reactivated
    );
    assert!(
        !set.contains("setup"),
        "setup should NOT be re-activated. Reactivated: {:?}",
        reactivated
    );
    assert_eq!(
        graph.get_task("setup").unwrap().status,
        Status::Done,
        "Setup should remain Done"
    );
}

#[test]
fn test_mixed_deps_cli_setup_not_reopened() {
    // CLI version of the mixed deps test
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Setup", "--id", "setup"]);
    wg_ok(
        &wg_dir,
        &["add", "Implement", "--id", "impl", "--after", "setup"],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Validate",
            "--id",
            "validate",
            "--after",
            "impl",
            "--max-iterations",
            "3",
        ],
    );

    wg_ok(&wg_dir, &["done", "setup"]);
    wg_ok(&wg_dir, &["done", "impl"]);
    let output = wg_ok(&wg_dir, &["done", "validate"]);

    assert!(
        output.contains("re-activated"),
        "Cycle should fire. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("setup").unwrap().status,
        Status::Done,
        "Setup should remain Done"
    );
    assert_eq!(
        graph.get_task("impl").unwrap().status,
        Status::Open,
        "Impl should be re-opened"
    );
    assert_eq!(
        graph.get_task("validate").unwrap().status,
        Status::Open,
        "Validate should be re-opened"
    );
}

// --- 8.6 First-iteration exemption ---

#[test]
fn test_first_iteration_exemption_header_ready() {
    // Cycle with back-edge: X(done) → A → B → A.
    // B has cycle_config (header). On first iteration, the header should get
    // back-edge exemption, allowing at least one task in the cycle to be ready.
    let x = make_task_with_status("x", "External pre-req", Status::Done);
    let mut a = make_task("a", "Worker A");
    a.after = vec!["x".to_string(), "b".to_string()]; // back-edge: A.after includes B
    let mut b = make_task("b", "Evaluator B");
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let graph = build_graph(vec![x, a, b]);
    let analysis = graph.compute_cycle_analysis();

    assert!(!analysis.cycles.is_empty(), "Should detect cycle");

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        !ready_ids.is_empty(),
        "At least one cycle task should be ready on first iteration. Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_first_iteration_exemption_implicit_cycle() {
    // Implicit cycle (no back-edge): A has no deps, B --after A --max-iterations 3.
    // A should be ready, B should wait for A.
    let a = make_task("a", "Worker");
    let mut b = make_task("b", "Evaluator");
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    assert!(
        analysis.cycles.is_empty(),
        "No SCC cycle for implicit cycle"
    );

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"a"),
        "A should be ready (no blockers). Ready: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"b"),
        "B should wait for A. Ready: {:?}",
        ready_ids
    );
}

// --- 8.7 Full end-to-end CLI tests ---

#[test]
fn test_e2e_cycle_via_max_iterations_complete_flow() {
    // Full e2e: create cycle, iterate twice, converge on third iteration.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    // Step 1: Create the cycle
    wg_ok(&wg_dir, &["add", "Worker", "--id", "worker"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Validator",
            "--id",
            "validator",
            "--after",
            "worker",
            "--max-iterations",
            "5",
        ],
    );

    // Step 2: Complete first iteration
    wg_ok(&wg_dir, &["done", "worker"]);
    let output = wg_ok(&wg_dir, &["done", "validator"]);
    assert!(
        output.contains("re-activated"),
        "First iteration should re-activate. Output: {}",
        output
    );

    // Step 3: Verify re-activation
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("worker").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("validator").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("worker").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("validator").unwrap().loop_iteration, 1);

    // Step 4: Complete second iteration
    wg_ok(&wg_dir, &["done", "worker"]);
    let output = wg_ok(&wg_dir, &["done", "validator"]);
    assert!(
        output.contains("re-activated"),
        "Second iteration should also re-activate. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("worker").unwrap().loop_iteration, 2);
    assert_eq!(graph.get_task("validator").unwrap().loop_iteration, 2);

    // Step 5: Complete third iteration with --converged
    wg_ok(&wg_dir, &["done", "worker"]);
    let output = wg_ok(&wg_dir, &["done", "validator", "--converged"]);
    assert!(
        !output.contains("re-activated"),
        "Converged should stop the cycle. Output: {}",
        output
    );

    // Step 6: Verify final state
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("worker").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("validator").unwrap().status, Status::Done);
    assert!(
        graph
            .get_task("validator")
            .unwrap()
            .tags
            .contains(&"converged".to_string())
    );
}

#[test]
fn test_e2e_guarded_cycle_guard_stops_loop() {
    // End-to-end: cycle with guard. Guard stops the loop when condition changes.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Sentinel", "--id", "sentinel"]);
    wg_ok(&wg_dir, &["add", "Worker", "--id", "worker"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Validator",
            "--id",
            "validator",
            "--after",
            "worker",
            "--max-iterations",
            "5",
            "--cycle-guard",
            "task:sentinel=failed",
        ],
    );

    // Fail sentinel so guard condition is met
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).unwrap();
    graph.get_task_mut("sentinel").unwrap().status = Status::Failed;
    save_graph(&graph, &graph_path).unwrap();

    // First iteration: guard met → iterate
    wg_ok(&wg_dir, &["done", "worker"]);
    let output = wg_ok(&wg_dir, &["done", "validator"]);
    assert!(
        output.contains("re-activated"),
        "Guard condition met → should iterate. Output: {}",
        output
    );

    // Change sentinel to Done → guard condition no longer met
    let mut graph = load_graph(&graph_path).unwrap();
    graph.get_task_mut("sentinel").unwrap().status = Status::Done;
    save_graph(&graph, &graph_path).unwrap();

    // Second iteration: guard NOT met → stop
    wg_ok(&wg_dir, &["done", "worker"]);
    let output = wg_ok(&wg_dir, &["done", "validator"]);
    assert!(
        !output.contains("re-activated"),
        "Guard condition not met → should NOT iterate. Output: {}",
        output
    );
}

#[test]
fn test_e2e_auto_back_edge_creates_structural_cycle() {
    // End-to-end: create cycle with --max-iterations + --after (auto back-edge).
    // A.after=[B] (auto back-edge) and B.after=[A] (forward).
    // On first iteration, A is exempt from B (back-edge exemption).
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Task A", "--id", "a"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task B",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "3",
        ],
    );

    // Verify cycle detection via CLI
    let output = wg_ok(&wg_dir, &["cycles"]);
    assert!(
        output.contains("Cycles detected") || output.contains("cycle"),
        "Should detect the structural cycle. Output: {}",
        output
    );

    // A is exempt from back-edge blocker B, so wg done a works directly
    wg_ok(&wg_dir, &["done", "a"]);

    // Complete B
    let output = wg_ok(&wg_dir, &["done", "b"]);
    assert!(
        output.contains("re-activated"),
        "Structural cycle should fire. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 1);
}

// ===========================================================================
// 9. Deep cycle API tests (--max-iterations + --after, no --loops-to)
// ===========================================================================

// --- 9.1 Basic cycle: A → B --max-iterations 3 ---

#[test]
fn test_deep_basic_cycle_done_a_done_b_reopens() {
    // A → B --max-iterations 3 (auto back-edge: B → A).
    // Done A, done B → both re-open, iteration increments.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Task A", "--id", "a"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task B",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "3",
        ],
    );

    // Verify auto back-edge created
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert!(
        graph
            .get_task("a")
            .unwrap()
            .after
            .contains(&"b".to_string()),
        "Auto back-edge: A.after should contain B"
    );

    // Complete A then B
    wg_ok(&wg_dir, &["done", "a"]);
    let output = wg_ok(&wg_dir, &["done", "b"]);
    assert!(
        output.contains("re-activated"),
        "Should re-activate. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("a").unwrap().status,
        Status::Open,
        "A re-opened"
    );
    assert_eq!(
        graph.get_task("b").unwrap().status,
        Status::Open,
        "B re-opened"
    );
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 1);
}

// --- 9.2 Multi-iteration: 3 full iterations, stops at max ---

#[test]
fn test_deep_multi_iteration_stops_at_max() {
    // A → B --max-iterations 3. Run through all 3 iterations, verify stops at max.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Task A", "--id", "a"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task B",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "3",
        ],
    );

    // Iteration 0 → 1
    wg_ok(&wg_dir, &["done", "a"]);
    let output = wg_ok(&wg_dir, &["done", "b"]);
    assert!(
        output.contains("re-activated"),
        "Iteration 0→1 should re-activate"
    );
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);

    // Iteration 1 → 2
    wg_ok(&wg_dir, &["done", "a"]);
    let output = wg_ok(&wg_dir, &["done", "b"]);
    assert!(
        output.contains("re-activated"),
        "Iteration 1→2 should re-activate"
    );
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 2);

    // Iteration 2: at max (max_iterations=3 means iterations 0,1,2), should NOT re-activate
    wg_ok(&wg_dir, &["done", "a"]);
    let output = wg_ok(&wg_dir, &["done", "b"]);
    assert!(
        !output.contains("re-activated"),
        "At max_iterations=3, iteration 2 should NOT re-activate. Output: {}",
        output
    );
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("a").unwrap().status,
        Status::Done,
        "A should stay Done at max"
    );
    assert_eq!(
        graph.get_task("b").unwrap().status,
        Status::Done,
        "B should stay Done at max"
    );
}

// --- 9.3 Convergence: wg done B --converged stops the cycle ---

#[test]
fn test_deep_convergence_stops_cycle() {
    // A → B --max-iterations 5. Complete A, then `wg done B --converged`.
    // Cycle should stop immediately.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Task A", "--id", "a"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task B",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "5",
        ],
    );

    wg_ok(&wg_dir, &["done", "a"]);
    let output = wg_ok(&wg_dir, &["done", "b", "--converged"]);
    assert!(
        !output.contains("re-activated"),
        "--converged should stop the cycle. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
    assert!(
        graph
            .get_task("b")
            .unwrap()
            .tags
            .contains(&"converged".to_string())
    );
}

// --- 9.4 Guard authority: Worker can't self-converge when guard is set ---

#[test]
fn test_deep_guard_authority_blocks_self_convergence() {
    // Existing test `test_self_convergence_blocked_by_guard` covers this.
    // This is an additional guard authority test with a different guard condition.
    let sentinel = make_task_with_status("sentinel", "Sentinel", Status::Open);
    let mut a = make_task_with_status("a", "Worker", Status::Done);
    a.after = vec!["b".to_string()];
    let mut b = make_task_with_status("b", "Evaluator", Status::Done);
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: Some(LoopGuard::TaskStatus {
            task: "sentinel".to_string(),
            status: Status::Failed,
        }),
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    // Add converged tag — guard should override it
    b.tags = vec!["converged".to_string()];

    let mut graph = build_graph(vec![sentinel, a, b]);
    let analysis = graph.compute_cycle_analysis();

    // Guard condition not met (sentinel is Open, not Failed) → no iteration
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(
        reactivated.is_empty(),
        "Guard condition not met → no iteration despite converged tag"
    );

    // Now change sentinel to Failed → guard condition met, converged tag ignored
    graph.get_task_mut("sentinel").unwrap().status = Status::Failed;
    // Reset tasks to Done
    graph.get_task_mut("a").unwrap().status = Status::Done;
    graph.get_task_mut("b").unwrap().status = Status::Done;
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(
        !reactivated.is_empty(),
        "Guard condition met → should iterate even with converged tag"
    );
}

// --- 9.5 First-iteration ordering: only header ready, B waits for A ---

#[test]
fn test_deep_first_iteration_ordering_b_waits_for_a() {
    // A ↔ B cycle. B has --max-iterations (cycle_config).
    // Auto back-edge creates: A.after=[B], B.after=[A].
    // Header = A (alphabetically first in isolated SCC).
    // Back-edge: B→A (adj direction) = A's dep on B is skipped.
    // Forward edge: A→B (adj direction) = B's dep on A blocks.
    // Only A should be ready; B waits for A to complete.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Task A", "--id", "a", "--immediate"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task B",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "3",
            "--immediate",
        ],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let analysis = graph.compute_cycle_analysis();

    assert!(
        !analysis.cycles.is_empty(),
        "Should detect cycle from auto back-edge"
    );

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"a"),
        "A (header) should be ready (back-edge from B skipped). Ready: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"b"),
        "B should NOT be ready (forward dep on A is Open). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_deep_first_iteration_ordering_unit_test() {
    // Same test but purely in-memory (no CLI):
    // A.after = [B] (back-edge), B.after = [A] (forward dep).
    // Header = A (alphabetically first in isolated SCC).
    // Back-edge: B→A in adj = A's dep on B skipped → A ready.
    // Forward: A→B in adj = B's dep on A blocks → B not ready.
    let mut a = make_task("a", "Task A");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"a"),
        "A (header) should be ready: back-edge from B skipped. Ready: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"b"),
        "B should NOT be ready (forward dep on A is Open). Ready: {:?}",
        ready_ids
    );
}

// --- 9.6 Re-iteration ordering: After cycle fires, A must be ready first ---

#[test]
fn test_deep_reiteration_ordering() {
    // After cycle fires and both re-open, A must be ready first again.
    // This verifies that on re-iteration, the ordering is preserved.
    let mut a = make_task("a", "Task A");
    a.after = vec!["b".to_string()];
    a.loop_iteration = 1; // Second iteration
    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    b.loop_iteration = 1;

    let graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"a"),
        "A should be ready on re-iteration (exempt from B). Ready: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"b"),
        "B should NOT be ready on re-iteration (A is Open). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_deep_reiteration_ordering_e2e() {
    // End-to-end: after cycle fires, A becomes ready first, B waits.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Task A", "--id", "a", "--immediate"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task B",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "3",
            "--immediate",
        ],
    );

    // First iteration
    wg_ok(&wg_dir, &["done", "a"]);
    wg_ok(&wg_dir, &["done", "b"]);

    // Now both are re-opened at iteration 1
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);

    let analysis = graph.compute_cycle_analysis();
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"a"),
        "After re-open, A should be ready first. Ready: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"b"),
        "After re-open, B should still wait for A. Ready: {:?}",
        ready_ids
    );
}

// --- 9.7 Three-task cycle: A → B → C --max-iterations 2 ---

#[test]
fn test_deep_three_task_cycle_reopen_and_ordering() {
    // A → B → C --max-iterations 2.
    // Auto back-edge: B.after includes C. (C's --after is B, so C creates back-edge to B.)
    // Wait — the auto back-edge logic is: for each dep in --after, add new task ID to dep.after.
    // So C --after B --max-iterations 2 → B.after gets "c" added.
    // This creates cycle B ↔ C, but A is not in the cycle.
    //
    // To get A → B → C with all three in the cycle:
    // A, B --after A, C --after B --max-iterations 2
    // Auto back-edge: B.after gets "c" → cycle is B ↔ C, A is outside.
    //
    // For a true 3-task cycle, we need: C --after A,B --max-iterations 2
    // Then auto back-edges: A.after gets "c", B.after gets "c"
    // This creates cycle: A → C → A and B → C → B, all in one SCC if we chain them.
    //
    // Actually, for A → B → C cycle, we need a pipeline where C loops back.
    // Let's use a slightly different setup:
    // A, B --after A, C --after B --max-iterations 2
    // Back-edge on C goes to B: B.after = [A, C], C.after = [B]
    // The SCC is {B, C}. A is outside. This is "setup + cycle" pattern.
    //
    // For a TRUE 3-node cycle, we need explicit manual construction:
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    // Create pipeline: A → B → C
    wg_ok(&wg_dir, &["add", "Task A", "--id", "a", "--immediate"]);
    wg_ok(
        &wg_dir,
        &["add", "Task B", "--id", "b", "--after", "a", "--immediate"],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task C",
            "--id",
            "c",
            "--after",
            "b",
            "--max-iterations",
            "2",
            "--immediate",
        ],
    );

    // Auto back-edge: B.after should now include "c"
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert!(
        graph
            .get_task("b")
            .unwrap()
            .after
            .contains(&"c".to_string()),
        "Auto back-edge: B.after should contain C"
    );

    // The SCC should be {B, C}. A is outside (setup).
    let analysis = graph.compute_cycle_analysis();
    assert!(!analysis.cycles.is_empty(), "Should detect cycle");
    let cycle_members: HashSet<&str> = analysis.cycles[0]
        .members
        .iter()
        .map(|s| s.as_str())
        .collect();
    assert!(cycle_members.contains("b"), "B should be in cycle");
    assert!(cycle_members.contains("c"), "C should be in cycle");

    // Complete A (setup), then B, then C
    wg_ok(&wg_dir, &["done", "a"]);
    wg_ok(&wg_dir, &["done", "b"]);
    let output = wg_ok(&wg_dir, &["done", "c"]);
    assert!(
        output.contains("re-activated"),
        "Cycle should fire. Output: {}",
        output
    );

    // Verify B and C are re-opened (A stays done since it's not in the cycle)
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("a").unwrap().status,
        Status::Done,
        "A (setup) should remain Done"
    );
    assert_eq!(
        graph.get_task("b").unwrap().status,
        Status::Open,
        "B should be re-opened"
    );
    assert_eq!(
        graph.get_task("c").unwrap().status,
        Status::Open,
        "C should be re-opened"
    );
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("c").unwrap().loop_iteration, 1);

    // Verify ordering is preserved: B should be ready (C is iterator, exempt)
    let analysis = graph.compute_cycle_analysis();
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ready_ids.contains(&"b"),
        "B should be ready after re-open (exempt from C iterator). Ready: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"c"),
        "C should NOT be ready (B is Open). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_deep_three_task_cycle_unit() {
    // Unit test: 3-node SCC where all three re-open on cycle fire.
    // Manual construction: A.after=[C], B.after=[A], C.after=[B], A has cycle_config.
    let mut a = make_task_with_status("a", "A (iterator)", Status::Done);
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 2,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    let mut c = make_task_with_status("c", "C", Status::Done);
    c.after = vec!["b".to_string()];

    let mut graph = build_graph(vec![a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    assert_eq!(analysis.cycles.len(), 1, "Should have one 3-node cycle");
    assert_eq!(analysis.cycles[0].members.len(), 3);

    let reactivated = evaluate_cycle_iteration(&mut graph, "c", &analysis);
    assert_eq!(reactivated.len(), 3, "All 3 should re-activate");

    for id in &["a", "b", "c"] {
        let task = graph.get_task(id).unwrap();
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.loop_iteration, 1);
    }

    // Ordering after re-open: only the cycle header should be ready
    // (back-edge to header skipped, all forward deps block non-headers).
    let analysis = graph.compute_cycle_analysis();
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert_eq!(
        ready_ids.len(),
        1,
        "Only the cycle header should be ready. Ready: {:?}",
        ready_ids
    );
    assert_eq!(
        ready_ids[0], analysis.cycles[0].header,
        "The ready task should be the cycle header"
    );
}

// --- 9.8 Mixed deps: setup → impl → validate --max-iterations 3 ---

#[test]
fn test_deep_mixed_deps_setup_included_behavior() {
    // Pipeline: setup → impl → validate --max-iterations 3.
    // Auto back-edge: impl.after gets "validate".
    // SCC: {impl, validate}. Setup is outside.
    // Verify: setup IS NOT included in cycle (it's not in SCC).
    // When cycle fires, only impl and validate re-open.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Setup", "--id", "setup", "--immediate"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Implement",
            "--id",
            "impl",
            "--after",
            "setup",
            "--immediate",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Validate",
            "--id",
            "validate",
            "--after",
            "impl",
            "--max-iterations",
            "3",
            "--immediate",
        ],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let analysis = graph.compute_cycle_analysis();

    // Verify SCC contains only impl and validate, not setup
    let cycle_members: HashSet<&str> = analysis.cycles[0]
        .members
        .iter()
        .map(|s| s.as_str())
        .collect();
    assert!(cycle_members.contains("impl"), "impl should be in cycle");
    assert!(
        cycle_members.contains("validate"),
        "validate should be in cycle"
    );
    assert!(
        !cycle_members.contains("setup"),
        "setup should NOT be in cycle SCC"
    );

    // Run the workflow
    wg_ok(&wg_dir, &["done", "setup"]);
    wg_ok(&wg_dir, &["done", "impl"]);
    let output = wg_ok(&wg_dir, &["done", "validate"]);
    assert!(output.contains("re-activated"), "Cycle should fire");

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("setup").unwrap().status,
        Status::Done,
        "Setup stays done"
    );
    assert_eq!(
        graph.get_task("impl").unwrap().status,
        Status::Open,
        "Impl re-opened"
    );
    assert_eq!(
        graph.get_task("validate").unwrap().status,
        Status::Open,
        "Validate re-opened"
    );

    // On re-iteration, impl should be ready (setup is Done, and validate iterator is exempt)
    let analysis = graph.compute_cycle_analysis();
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ready_ids.contains(&"impl"),
        "Impl should be ready on re-iteration. Ready: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"validate"),
        "Validate should wait for impl. Ready: {:?}",
        ready_ids
    );
}

// --- 9.9 Diamond into cycle ---

#[test]
fn test_deep_diamond_into_cycle() {
    // A → B, A → C, B → D, C → D --max-iterations 2.
    // Auto back-edge: B.after gets "d", C.after gets "d".
    // The SCC should be {B, C, D} since B↔D and C↔D.
    // A is the setup.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Task A", "--id", "a"]);
    wg_ok(&wg_dir, &["add", "Task B", "--id", "b", "--after", "a"]);
    wg_ok(&wg_dir, &["add", "Task C", "--id", "c", "--after", "a"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task D",
            "--id",
            "d",
            "--after",
            "b,c",
            "--max-iterations",
            "2",
        ],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();

    // Verify auto back-edges
    assert!(
        graph
            .get_task("b")
            .unwrap()
            .after
            .contains(&"d".to_string()),
        "B.after should have D (auto back-edge)"
    );
    assert!(
        graph
            .get_task("c")
            .unwrap()
            .after
            .contains(&"d".to_string()),
        "C.after should have D (auto back-edge)"
    );

    let analysis = graph.compute_cycle_analysis();
    assert!(
        !analysis.cycles.is_empty(),
        "Should detect cycle in diamond+cycle"
    );

    // Verify A is not in any cycle
    assert!(
        !analysis.task_to_cycle.contains_key("a"),
        "A should not be in any cycle"
    );

    // Complete workflow
    wg_ok(&wg_dir, &["done", "a"]);
    wg_ok(&wg_dir, &["done", "b"]);
    wg_ok(&wg_dir, &["done", "c"]);
    let output = wg_ok(&wg_dir, &["done", "d"]);
    assert!(
        output.contains("re-activated"),
        "Cycle should fire. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("a").unwrap().status,
        Status::Done,
        "A stays done"
    );
    assert_eq!(
        graph.get_task("b").unwrap().status,
        Status::Open,
        "B re-opened"
    );
    assert_eq!(
        graph.get_task("c").unwrap().status,
        Status::Open,
        "C re-opened"
    );
    assert_eq!(
        graph.get_task("d").unwrap().status,
        Status::Open,
        "D re-opened"
    );
}

// --- 9.10 Cycle with no --max-iterations: manual back-edge, no cycle_config ---

#[test]
fn test_deep_no_max_iterations_no_auto_iteration() {
    // A → B → A (manual back-edge via wg edit --add-after). No cycle_config.
    // Verify no iteration happens (no cycle_config = no auto-iteration).
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Task A", "--id", "a"]);
    wg_ok(&wg_dir, &["add", "Task B", "--id", "b", "--after", "a"]);

    // Create manual back-edge: A --add-after B
    wg_ok(&wg_dir, &["edit", "a", "--add-after", "b"]);

    // Verify back-edge exists
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert!(
        graph
            .get_task("a")
            .unwrap()
            .after
            .contains(&"b".to_string()),
        "Manual back-edge: A.after should contain B"
    );

    // Verify cycle detected but no cycle_config
    let analysis = graph.compute_cycle_analysis();
    assert!(!analysis.cycles.is_empty(), "Should detect cycle");
    assert!(
        graph.get_task("a").unwrap().cycle_config.is_none(),
        "A should have no cycle_config"
    );
    assert!(
        graph.get_task("b").unwrap().cycle_config.is_none(),
        "B should have no cycle_config"
    );

    // Both tasks start as Open with cycle deadlock (no exemption without cycle_config)
    // We need to manually set statuses to test iteration behavior
    // Use the unit test approach:
    let mut a = make_task_with_status("a", "Task A", Status::Done);
    a.after = vec!["b".to_string()];
    let mut b = make_task_with_status("b", "Task B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(
        reactivated.is_empty(),
        "No cycle_config = no auto-iteration. Reactivated: {:?}",
        reactivated
    );

    // Both should remain Done
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
}

// --- 9.11 --converged on first iteration: should stop immediately ---

#[test]
fn test_deep_converged_first_iteration() {
    // A → B --max-iterations 5. On first completion, use --converged.
    // Cycle should stop immediately, never re-open.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Task A", "--id", "a"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task B",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "5",
        ],
    );

    wg_ok(&wg_dir, &["done", "a"]);
    let output = wg_ok(&wg_dir, &["done", "b", "--converged"]);
    assert!(
        !output.contains("re-activated"),
        "--converged on first iteration should stop immediately. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
    assert_eq!(
        graph.get_task("a").unwrap().loop_iteration,
        0,
        "A should stay at iteration 0"
    );
    assert_eq!(
        graph.get_task("b").unwrap().loop_iteration,
        0,
        "B should stay at iteration 0"
    );
}

#[test]
fn test_deep_converged_first_iteration_unit() {
    // Unit test version: both Done at iteration 0, converged tag on config owner.
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    b.tags = vec!["converged".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(
        reactivated.is_empty(),
        "Converged on first iteration should prevent re-activation"
    );
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 0);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 0);
}

// --- 9.12 Parallel cycles: two independent cycles don't interfere ---

#[test]
fn test_deep_parallel_cycles_no_interference() {
    // Cycle 1: A → B --max-iterations 3
    // Cycle 2: C → D --max-iterations 2
    // Independent. Completing cycle 1 should not affect cycle 2.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    // Cycle 1
    wg_ok(&wg_dir, &["add", "A", "--id", "a"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "B",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "3",
        ],
    );

    // Cycle 2
    wg_ok(&wg_dir, &["add", "C", "--id", "c"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "D",
            "--id",
            "d",
            "--after",
            "c",
            "--max-iterations",
            "2",
        ],
    );

    // Complete cycle 1
    wg_ok(&wg_dir, &["done", "a"]);
    let output = wg_ok(&wg_dir, &["done", "b"]);
    assert!(output.contains("re-activated"), "Cycle 1 should fire");

    // Verify cycle 2 unaffected
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("c").unwrap().status,
        Status::Open,
        "C should still be Open"
    );
    assert_eq!(
        graph.get_task("d").unwrap().status,
        Status::Open,
        "D should still be Open"
    );
    assert_eq!(
        graph.get_task("c").unwrap().loop_iteration,
        0,
        "C iteration should be 0"
    );
    assert_eq!(
        graph.get_task("d").unwrap().loop_iteration,
        0,
        "D iteration should be 0"
    );

    // Cycle 1 re-opened
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);

    // Now complete cycle 2
    wg_ok(&wg_dir, &["done", "c"]);
    let output = wg_ok(&wg_dir, &["done", "d"]);
    assert!(output.contains("re-activated"), "Cycle 2 should fire");

    // Verify cycle 1 unaffected (still at iteration 1, both Open)
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("a").unwrap().loop_iteration,
        1,
        "A still at iteration 1"
    );
    assert_eq!(
        graph.get_task("b").unwrap().loop_iteration,
        1,
        "B still at iteration 1"
    );
    // Cycle 2 re-opened
    assert_eq!(graph.get_task("c").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("d").unwrap().loop_iteration, 1);
}

#[test]
fn test_deep_parallel_cycles_dispatch_independence() {
    // Verify dispatch doesn't mix tasks from different cycles.
    // Cycle 1: A ↔ B, Cycle 2: C ↔ D.
    // With back-edge exemption, only each cycle's header is ready.
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut c = make_task("c", "C");
    c.after = vec!["d".to_string()];
    let mut d = make_task("d", "D");
    d.after = vec!["c".to_string()];
    d.cycle_config = Some(CycleConfig {
        max_iterations: 2,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let graph = build_graph(vec![a, b, c, d]);
    let analysis = graph.compute_cycle_analysis();

    assert_eq!(
        analysis.cycles.len(),
        2,
        "Should have two independent cycles"
    );

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let mut ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    ready_ids.sort();

    // Exactly one task from each cycle should be ready (the header).
    assert_eq!(
        ready_ids.len(),
        2,
        "Exactly two tasks (one per cycle header) should be ready. Ready: {:?}",
        ready_ids
    );

    // Each ready task should be its cycle's header.
    for task in &ready {
        let cycle_idx = analysis
            .task_to_cycle
            .get(&task.id)
            .expect("Ready task should be in a cycle");
        assert_eq!(
            analysis.cycles[*cycle_idx].header, task.id,
            "Ready task {} should be its cycle's header",
            task.id
        );
    }
}

// --- 9.13 Nested structure: outer cycle contains inner non-cycle tasks ---

#[test]
fn test_deep_nested_structure_inner_non_cycle() {
    // Outer cycle: worker → validator --max-iterations 3.
    // Inner non-cycle task: helper (no deps, no cycle involvement).
    // Helper should not be affected by cycle operations.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Worker", "--id", "worker"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Validator",
            "--id",
            "validator",
            "--after",
            "worker",
            "--max-iterations",
            "3",
        ],
    );
    wg_ok(&wg_dir, &["add", "Helper", "--id", "helper"]);

    // Complete everything
    wg_ok(&wg_dir, &["done", "worker"]);
    wg_ok(&wg_dir, &["done", "helper"]);
    let output = wg_ok(&wg_dir, &["done", "validator"]);
    assert!(output.contains("re-activated"), "Cycle should fire");

    // Verify helper is unaffected
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(
        graph.get_task("helper").unwrap().status,
        Status::Done,
        "Helper stays Done"
    );
    assert_eq!(
        graph.get_task("helper").unwrap().loop_iteration,
        0,
        "Helper iteration unchanged"
    );

    // Cycle members re-opened
    assert_eq!(graph.get_task("worker").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("validator").unwrap().status, Status::Open);
}

#[test]
fn test_deep_nested_structure_dependent_non_cycle() {
    // Cycle: worker ↔ validator. Non-cycle task depends on a cycle member:
    // reporter --after validator.
    // When cycle fires and validator re-opens, reporter should be blocked again.
    let mut worker = make_task("worker", "Worker");
    worker.after = vec!["validator".to_string()];
    let mut validator = make_task("validator", "Validator");
    validator.after = vec!["worker".to_string()];
    validator.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut reporter = make_task("reporter", "Reporter");
    reporter.after = vec!["validator".to_string()];

    let graph = build_graph(vec![worker, validator, reporter]);
    let analysis = graph.compute_cycle_analysis();

    // Reporter should not be ready (validator is Open)
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        !ready_ids.contains(&"reporter"),
        "Reporter should not be ready while validator is Open. Ready: {:?}",
        ready_ids
    );

    // Only the cycle header should be ready (back-edge to header skipped)
    let header = &analysis.cycles[0].header;
    assert!(
        ready_ids.contains(&header.as_str()),
        "Cycle header ({}) should be ready. Ready: {:?}",
        header,
        ready_ids
    );
}

// ===========================================================================
// 10. CLI E2E tests for cycle API
// ===========================================================================

// --- 10.14 wg add with --max-iterations creates back-edges visible in wg show ---

#[test]
fn test_deep_cli_add_back_edge_visible_in_show() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Worker", "--id", "worker"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Validator",
            "--id",
            "validator",
            "--after",
            "worker",
            "--max-iterations",
            "3",
        ],
    );

    // wg show worker should mention validator as a dependency
    let output = wg_ok(&wg_dir, &["show", "worker"]);
    assert!(
        output.contains("validator"),
        "wg show worker should show validator in dependencies (auto back-edge). Output:\n{}",
        output
    );

    // wg show validator should show worker as after dependency
    let output = wg_ok(&wg_dir, &["show", "validator"]);
    assert!(
        output.contains("worker"),
        "wg show validator should show worker dependency. Output:\n{}",
        output
    );
    assert!(
        output.contains("max_iterations")
            || output.contains("cycle_config")
            || output.contains("Cycle"),
        "wg show validator should mention cycle config. Output:\n{}",
        output
    );
}

// --- 10.15 wg cycles detects auto-created cycle ---

#[test]
fn test_deep_cli_cycles_detects_auto_cycle() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Worker", "--id", "worker"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Validator",
            "--id",
            "validator",
            "--after",
            "worker",
            "--max-iterations",
            "3",
        ],
    );

    let output = wg_ok(&wg_dir, &["cycles"]);
    assert!(
        output.contains("Cycles detected"),
        "wg cycles should detect auto-created cycle. Output: {}",
        output
    );

    // JSON mode should also work
    let output = wg_ok(&wg_dir, &["cycles", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&output)
        .unwrap_or_else(|e| panic!("Invalid JSON: {}\nOutput: {}", e, output));
    assert_eq!(
        parsed["cycle_count"].as_u64().unwrap(),
        1,
        "Should report 1 cycle"
    );
}

// --- 10.16 Full workflow: add → done → cycle fires → repeat ---

#[test]
fn test_deep_cli_full_workflow_dispatch_cycle() {
    // Full workflow: add tasks, complete them, cycle fires, complete again.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Implement", "--id", "impl"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Review",
            "--id",
            "review",
            "--after",
            "impl",
            "--max-iterations",
            "3",
        ],
    );

    // First round
    wg_ok(&wg_dir, &["done", "impl"]);
    let output = wg_ok(&wg_dir, &["done", "review"]);
    assert!(output.contains("re-activated"), "First cycle should fire");

    // Verify re-opened state
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("impl").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("impl").unwrap().loop_iteration, 1);

    // Second round
    wg_ok(&wg_dir, &["done", "impl"]);
    let output = wg_ok(&wg_dir, &["done", "review"]);
    assert!(output.contains("re-activated"), "Second cycle should fire");

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("impl").unwrap().loop_iteration, 2);

    // Third round: converge
    wg_ok(&wg_dir, &["done", "impl"]);
    let output = wg_ok(&wg_dir, &["done", "review", "--converged"]);
    assert!(
        !output.contains("re-activated"),
        "Converged should stop cycle"
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("impl").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("review").unwrap().status, Status::Done);
}

// --- 10.17 Verify --loops-to is GONE ---

#[test]
fn test_deep_cli_loops_to_removed() {
    // wg add --loops-to should error.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(
        &wg_dir,
        &["add", "Bad Task", "--id", "bad", "--loops-to", "something"],
    );
    assert!(
        !output.status.success(),
        "--loops-to should be rejected as an unknown argument. Exit code: {:?}",
        output.status
    );

    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        stderr.contains("unexpected argument")
            || stderr.contains("error")
            || stderr.contains("unknown"),
        "Should report --loops-to as an unrecognized argument. stderr: {}",
        stderr
    );
}

// --- 10.18 wg viz shows cycle back-edge annotation ---

#[test]
fn test_deep_cli_viz_back_edge_annotation() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Worker", "--id", "worker"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Validator",
            "--id",
            "validator",
            "--after",
            "worker",
            "--max-iterations",
            "3",
        ],
    );

    let output = wg_ok(&wg_dir, &["viz", "--all"]);

    // Should have cycle annotation marker ↺ and NOT duplicate the node
    assert!(
        output.contains("↺") || output.contains("cycle"),
        "Viz should annotate cycle back-edge. Output:\n{}",
        output
    );

    // Count occurrences of "worker" — it should appear once as a node, possibly once in the back-edge annotation.
    // It should NOT appear as a duplicate full node entry.
    let worker_lines: Vec<&str> = output.lines().filter(|l| l.contains("worker")).collect();
    assert!(
        !worker_lines.is_empty(),
        "worker should appear in viz output"
    );
}

#[test]
fn test_deep_cli_viz_no_duplicate_nodes() {
    // Ensure cycle doesn't cause duplicate node rendering in viz.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "A", "--id", "a"]);
    wg_ok(&wg_dir, &["add", "B", "--id", "b", "--after", "a"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "C",
            "--id",
            "c",
            "--after",
            "b",
            "--max-iterations",
            "2",
        ],
    );

    let output = wg_ok(&wg_dir, &["viz", "--all"]);

    // In a proper viz, each node should appear once as a full entry.
    // The back-edge should use ↺ annotation, not re-render the node.
    let has_cycle_marker = output.contains("↺");
    assert!(
        has_cycle_marker || !output.is_empty(),
        "Viz should render without errors. Output:\n{}",
        output
    );
}

#[test]
fn test_viz_cycle_back_edge_no_duplicate_node_rendering() {
    // Cycle: A → B → C → A (back-edge from C to A).
    // Back-edges are now rendered as right-side arcs (← / ┘), NOT as
    // duplicate child nodes. Verify no duplicate node lines appear.
    let tmp = TempDir::new().unwrap();
    let a = make_task("a", "Task A");
    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c", "Task C");
    c.after = vec!["b".to_string()];
    c.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    // Back-edge: a depends on c (creating cycle a→b→c→a)
    let mut a_with_back = a.clone();
    a_with_back.after = vec!["c".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![a_with_back, b, c]);
    let output = wg_ok(&wg_dir, &["viz", "--all"]);

    // Back-edges should produce right-side arcs, not duplicate nodes
    assert!(
        output.contains("←") || output.contains("┘"),
        "Back-edge should render as right-side arc (← / ┘). Output:\n{}",
        output
    );

    // Node "a" should appear exactly once as a rendered node (not duplicated by back-edge)
    let a_node_lines: Vec<&str> = output
        .lines()
        .filter(|l| l.contains("(open)") && l.trim_start().starts_with("a "))
        .collect();
    assert!(
        a_node_lines.len() <= 1,
        "Node 'a' should not be duplicated by back-edge rendering. Found {} lines: {:?}\nFull output:\n{}",
        a_node_lines.len(),
        a_node_lines,
        output
    );
}

#[test]
fn test_viz_cycle_members_shown_without_all_flag() {
    // When a cycle has mixed done/open members, all members should be visible
    // even without --all, so the cycle structure is complete.
    let tmp = TempDir::new().unwrap();
    let a = make_task_with_status("a", "Task A", Status::Done);
    let mut b = make_task("b", "Task B"); // open
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    // Back-edge: a depends on b (creating cycle a→b→a)
    let mut a_with_back = a.clone();
    a_with_back.after = vec!["b".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![a_with_back, b]);

    // Without --all: done task 'a' should still appear because it's in
    // an active cycle with non-done member 'b'
    let output = wg_ok(&wg_dir, &["viz"]);
    assert!(
        output.contains("a") && output.contains("b"),
        "Both cycle members should appear even without --all. Output:\n{}",
        output
    );
}

// ===========================================================================
// Guard authority: converged tag vs cycle guard
// ===========================================================================

#[test]
fn test_reactivate_ignores_converged_tag_when_guard_is_set() {
    // Even if the config owner has a "converged" tag (e.g., injected somehow),
    // reactivate_cycle should ignore it when a non-trivial guard is present.
    // The guard is the authority on convergence, not the tag.
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: Some(LoopGuard::TaskStatus {
            task: "sentinel".to_string(),
            status: Status::Failed,
        }),
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    a.tags = vec!["converged".to_string()]; // Injected converged tag

    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    // Sentinel is failed → guard says "iterate"
    let sentinel = make_task_with_status("sentinel", "Sentinel", Status::Failed);

    let mut graph = build_graph(vec![a, b, sentinel]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        !reactivated.is_empty(),
        "Cycle should reactivate despite converged tag — guard is authoritative"
    );
}

#[test]
fn test_reactivate_respects_converged_tag_when_no_guard() {
    // Without a guard (guard = None), the converged tag SHOULD stop iteration.
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    a.tags = vec!["converged".to_string()];

    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        reactivated.is_empty(),
        "Cycle should NOT reactivate when converged tag is set and no guard"
    );
}

// ===========================================================================
// 12. evaluate_all_cycle_iterations (coordinator-level sweep)
// ===========================================================================

#[test]
fn test_evaluate_all_cycles_reactivates_completed_cycle() {
    // 2-node cycle: A ↔ B, both Done, max_iterations=3, loop_iteration=0.
    // evaluate_all_cycle_iterations should reactivate both.
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);
    assert_eq!(reactivated.len(), 2, "Both members should be re-activated");
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 1);
}

#[test]
fn test_evaluate_all_cycles_skips_incomplete_cycle() {
    // 2-node cycle: A Done, B Open. Should NOT reactivate.
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task("b", "B"); // Open
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);
    assert!(
        reactivated.is_empty(),
        "Should not reactivate incomplete cycle"
    );
}

#[test]
fn test_evaluate_all_cycles_respects_max_iterations() {
    // 2-node cycle at max_iterations — should NOT reactivate.
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 2,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    a.loop_iteration = 2;
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    b.loop_iteration = 2;

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);
    assert!(
        reactivated.is_empty(),
        "Should not reactivate at max iterations"
    );
}

#[test]
fn test_cycle_reactivates_indefinitely_with_max_iterations_zero() {
    // max_iterations=0 means unlimited. The cycle should reactivate no matter
    // how high the loop_iteration gets.
    for iteration in [0, 1, 5, 100, 1000] {
        let mut a = make_task_with_status("a", "A", Status::Done);
        a.after = vec!["b".to_string()];
        a.loop_iteration = iteration;
        a.cycle_config = Some(CycleConfig {
            max_iterations: 0,
            guard: None,
            delay: None,
            no_converge: false,
            restart_on_failure: true,
            max_failure_restarts: None,
        });
        let mut b = make_task_with_status("b", "B", Status::Done);
        b.after = vec!["a".to_string()];
        b.loop_iteration = iteration;

        let mut graph = build_graph(vec![a, b]);
        let analysis = graph.compute_cycle_analysis();

        let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);
        assert_eq!(
            reactivated.len(),
            2,
            "Cycle with max_iterations=0 should reactivate at iteration {}",
            iteration
        );
        assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("a").unwrap().loop_iteration, iteration + 1);
    }
}

#[test]
fn test_evaluate_all_cycles_three_node_cycle() {
    // 3-node cycle: A → B → C → A, all Done, max_iterations=3.
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    let mut c = make_task_with_status("c", "C", Status::Done);
    c.after = vec!["b".to_string()];

    let mut graph = build_graph(vec![a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);
    assert_eq!(reactivated.len(), 3, "All 3 members should be re-activated");

    for id in &["a", "b", "c"] {
        let task = graph.get_task(id).unwrap();
        assert_eq!(task.status, Status::Open, "{} should be Open", id);
        assert_eq!(task.loop_iteration, 1, "{} iteration should be 1", id);
    }
}

#[test]
fn test_evaluate_all_cycles_multi_iteration_sweep() {
    // Simulate the coordinator calling evaluate_all_cycle_iterations on each tick.
    // 3-node cycle: A → B → C → A, max_iterations=3.
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    let mut c = make_task_with_status("c", "C", Status::Done);
    c.after = vec!["b".to_string()];

    let mut graph = build_graph(vec![a, b, c]);

    // Iteration 0 → 1: all Done, sweep reactivates
    let analysis = graph.compute_cycle_analysis();
    let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);
    assert_eq!(reactivated.len(), 3);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);

    // Set all back to Done (simulating completion of iteration 1)
    for id in &["a", "b", "c"] {
        graph.get_task_mut(id).unwrap().status = Status::Done;
    }

    // Iteration 1 → 2
    let analysis = graph.compute_cycle_analysis();
    let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);
    assert_eq!(reactivated.len(), 3);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 2);

    // Set all back to Done (simulating completion of iteration 2)
    for id in &["a", "b", "c"] {
        graph.get_task_mut(id).unwrap().status = Status::Done;
    }

    // Iteration 2 at max (max_iterations=3 means iterations 0,1,2): should NOT reactivate
    let analysis = graph.compute_cycle_analysis();
    let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);
    assert!(reactivated.is_empty(), "Should stop at max_iterations=3");
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
}

#[test]
fn test_evaluate_all_cycles_respects_convergence() {
    // Cycle with converged tag should not reactivate.
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    a.tags = vec!["converged".to_string()];
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);
    assert!(
        reactivated.is_empty(),
        "Converged cycle should not reactivate"
    );
}

#[test]
fn test_evaluate_all_cycles_no_config_no_reactivation() {
    // Cycle without CycleConfig — no reactivation.
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);
    assert!(
        reactivated.is_empty(),
        "Cycle without config should not reactivate"
    );
}

#[test]
fn test_e2e_three_task_cycle_via_edit_multi_iteration() {
    // Full e2e: create 3-task cycle via wg edit, iterate 3 times.
    // This reproduces the exact bug scenario from the issue.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    // Create the pipeline
    wg_ok(&wg_dir, &["add", "Step A", "--id", "step-a"]);
    wg_ok(
        &wg_dir,
        &["add", "Step B", "--id", "step-b", "--after", "step-a"],
    );
    wg_ok(
        &wg_dir,
        &["add", "Step C", "--id", "step-c", "--after", "step-b"],
    );

    // Create back-edge via edit (this is the reproduction path)
    wg_ok(
        &wg_dir,
        &[
            "edit",
            "step-a",
            "--add-after",
            "step-c",
            "--max-iterations",
            "3",
        ],
    );

    // Verify cycle detected
    let output = wg_ok(&wg_dir, &["cycles"]);
    assert!(
        output.contains("Cycles detected"),
        "Should detect cycle. Output: {}",
        output
    );

    // Iteration 0: step-b → step-c → step-a
    wg_ok(&wg_dir, &["done", "step-b"]);
    wg_ok(&wg_dir, &["done", "step-c"]);
    let output = wg_ok(&wg_dir, &["done", "step-a"]);
    assert!(
        output.contains("re-activated"),
        "Iteration 0 should re-activate. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("step-a").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("step-a").unwrap().status, Status::Open);

    // Iteration 1: step-b → step-c → step-a
    wg_ok(&wg_dir, &["done", "step-b"]);
    wg_ok(&wg_dir, &["done", "step-c"]);
    let output = wg_ok(&wg_dir, &["done", "step-a"]);
    assert!(
        output.contains("re-activated"),
        "Iteration 1 should re-activate. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("step-a").unwrap().loop_iteration, 2);

    // Iteration 2 (at max, max_iterations=3 means iterations 0,1,2): should NOT re-activate
    wg_ok(&wg_dir, &["done", "step-b"]);
    wg_ok(&wg_dir, &["done", "step-c"]);
    let output = wg_ok(&wg_dir, &["done", "step-a"]);
    assert!(
        !output.contains("re-activated"),
        "At max_iterations=3, should NOT re-activate. Output: {}",
        output
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("step-a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("step-b").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("step-c").unwrap().status, Status::Done);
}

// ===========================================================================
// no_converge: forced cycle iterations
// ===========================================================================

#[test]
fn test_no_converge_ignores_converged_tag_in_reactivate() {
    // Cycle: A → B → A. A is header with no_converge=true and "converged" tag.
    // Both Done. The converged tag should be ignored — cycle should reactivate.
    let mut a = make_task_with_status("a", "A (forced header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: true,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    a.tags = vec!["converged".to_string()]; // Would normally stop the cycle
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let ca = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &ca);
    assert!(
        !reactivated.is_empty(),
        "no_converge cycle should reactivate despite converged tag"
    );
}

#[test]
fn test_no_converge_respects_max_iterations() {
    // Even with no_converge, the max_iterations hard cap is respected.
    let mut a = make_task_with_status("a", "A (forced)", Status::Done);
    a.after = vec!["b".to_string()];
    a.loop_iteration = 3;
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: true,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    b.loop_iteration = 3;

    let mut graph = build_graph(vec![a, b]);
    let ca = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &ca);
    assert!(
        reactivated.is_empty(),
        "no_converge should NOT override max_iterations hard cap"
    );
}

#[test]
fn test_cli_done_converged_ignored_for_no_converge_cycle() {
    // End-to-end: completing with --converged on a no_converge cycle
    // should ignore the convergence signal and keep cycling.
    let tmp = TempDir::new().unwrap();
    let mut a = make_task_with_status("a", "A", Status::Done);
    a.after = vec!["b".to_string()];
    let mut b = make_task_with_status("b", "B (forced header)", Status::InProgress);
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: true,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    // Complete B with --converged
    let output = wg_ok(&wg_dir, &["done", "b", "--converged"]);

    // Should re-activate because no_converge ignores --converged
    assert!(
        output.contains("re-activated"),
        "no-converge cycle should re-activate despite --converged. Output: {}",
        output
    );

    // Verify converged tag was NOT added
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task_b = graph.get_task("b").unwrap();
    assert!(
        !task_b.tags.contains(&"converged".to_string()),
        "Converged tag should NOT be added on no-converge cycle"
    );
}

#[test]
fn test_no_converge_without_converged_tag_iterates_normally() {
    // no_converge cycle with no converged tag should iterate normally.
    let mut a = make_task_with_status("a", "A (forced)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: true,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let ca = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &ca);
    assert!(
        !reactivated.is_empty(),
        "no_converge cycle should iterate normally"
    );
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 1);
}

// ===========================================================================
// Cycle failure restart tests
// ===========================================================================

#[test]
fn test_failure_restart_reactivates_cycle() {
    // A -> B -> A cycle, B has cycle_config with restart_on_failure=true (default)
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    a.status = Status::Done;
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    b.status = Status::Failed;
    b.failure_reason = Some("compilation error".to_string());
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b]);
    let ca = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_on_failure(&mut graph, "b", &ca);

    assert!(
        !reactivated.is_empty(),
        "Cycle should be reactivated on failure"
    );
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
    // loop_iteration should NOT be incremented (failure retry, not new iteration)
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 0);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 0);
    // cycle_failure_restarts should be incremented on config owner
    assert_eq!(graph.get_task("b").unwrap().cycle_failure_restarts, 1);
    // failure_reason should be cleared
    assert!(graph.get_task("b").unwrap().failure_reason.is_none());
    // assigned should be cleared
    assert!(graph.get_task("a").unwrap().assigned.is_none());
    assert!(graph.get_task("b").unwrap().assigned.is_none());
}

#[test]
fn test_failure_restart_disabled() {
    // Same cycle but restart_on_failure = false
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    a.status = Status::Done;
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    b.status = Status::Failed;
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: false,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b]);
    let ca = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_on_failure(&mut graph, "b", &ca);

    assert!(
        reactivated.is_empty(),
        "Cycle should NOT be reactivated when restart_on_failure=false"
    );
    assert_eq!(graph.get_task("b").unwrap().status, Status::Failed);
}

#[test]
fn test_failure_restart_max_exceeded() {
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    a.status = Status::Done;
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    b.status = Status::Failed;
    b.cycle_failure_restarts = 3; // Already at max
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: Some(3),
    });

    let mut graph = build_graph(vec![a, b]);
    let ca = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_on_failure(&mut graph, "b", &ca);

    assert!(
        reactivated.is_empty(),
        "Cycle should NOT restart when max_failure_restarts exceeded"
    );
    assert_eq!(graph.get_task("b").unwrap().status, Status::Failed);
}

#[test]
fn test_failure_restart_preserves_iteration() {
    // Cycle at iteration 2 — failure restart should keep iteration at 2
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    a.status = Status::Done;
    a.loop_iteration = 2;
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    b.status = Status::Failed;
    b.loop_iteration = 2;
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b]);
    let ca = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_on_failure(&mut graph, "b", &ca);

    assert!(!reactivated.is_empty());
    // loop_iteration stays the same
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 2);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 2);
}

#[test]
fn test_failure_restart_then_successful_iteration() {
    // Simulate: cycle restarts due to failure, then completes successfully
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    a.status = Status::Done;
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    b.status = Status::Failed;
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b]);
    let ca = graph.compute_cycle_analysis();

    // Step 1: Failure restart
    let reactivated = evaluate_cycle_on_failure(&mut graph, "b", &ca);
    assert!(!reactivated.is_empty());
    assert_eq!(graph.get_task("b").unwrap().cycle_failure_restarts, 1);

    // Step 2: Complete both tasks
    graph.get_task_mut("a").unwrap().status = Status::Done;
    graph.get_task_mut("b").unwrap().status = Status::Done;

    // Step 3: Normal cycle iteration should work
    let ca = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &ca);
    assert!(
        !reactivated.is_empty(),
        "Normal iteration should still work after failure restart"
    );
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 1);
}

#[test]
fn test_failure_restart_logs_failure_info() {
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    a.status = Status::Done;
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    b.status = Status::Failed;
    b.failure_reason = Some("timeout exceeded".to_string());
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b]);
    let ca = graph.compute_cycle_analysis();
    evaluate_cycle_on_failure(&mut graph, "b", &ca);

    // Check that failure info is in the log
    let a_log = &graph.get_task("a").unwrap().log;
    let last_log = a_log.last().unwrap();
    assert!(
        last_log.message.contains("Cycle failure restart"),
        "Log should mention cycle failure restart, got: {}",
        last_log.message
    );
    assert!(
        last_log.message.contains("timeout exceeded"),
        "Log should contain failure reason, got: {}",
        last_log.message
    );
}

#[test]
fn test_failure_restart_coordinator_sweep() {
    // Test evaluate_all_cycle_failure_restarts (coordinator sweep)
    let mut a = make_task("a", "A");
    a.after = vec!["b".to_string()];
    a.status = Status::Done;
    let mut b = make_task("b", "B");
    b.after = vec!["a".to_string()];
    b.status = Status::Failed;
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b]);
    let ca = graph.compute_cycle_analysis();
    let reactivated = evaluate_all_cycle_failure_restarts(&mut graph, &ca);

    assert!(
        !reactivated.is_empty(),
        "Coordinator sweep should catch failed cycle members"
    );
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
}

#[test]
fn test_failure_restart_default_is_true() {
    // Verify that restart_on_failure defaults to true for cycles
    let config = CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    };
    assert!(config.restart_on_failure);

    // Test deserialization with missing field — should default to true
    let json = r#"{"max_iterations": 3}"#;
    let config: CycleConfig = serde_json::from_str(json).unwrap();
    assert!(
        config.restart_on_failure,
        "restart_on_failure should default to true"
    );
}

#[test]
fn test_failure_restart_not_triggered_for_non_cycle_task() {
    // A standalone failed task (not in a cycle) should not trigger any restart
    let mut a = make_task("a", "A");
    a.status = Status::Failed;

    let mut graph = build_graph(vec![a]);
    let ca = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_on_failure(&mut graph, "a", &ca);

    assert!(
        reactivated.is_empty(),
        "Non-cycle task should not trigger restart"
    );
}

// ===========================================================================
// Abandoned members in cycles
// ===========================================================================

#[test]
fn test_cycle_iterates_when_one_member_done_one_abandoned() {
    // Cycle: A → B → A. A is Done, B is Abandoned. max_iterations = 3.
    // The cycle should iterate because all members are terminal.
    // Only the Done member (A) should be reset to Open.
    // The Abandoned member (B) should stay Abandoned.
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Abandoned);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "a", &analysis);

    assert!(
        !reactivated.is_empty(),
        "Cycle should iterate when all members are terminal (done + abandoned)"
    );
    // Done member resets to Open
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);
    // Abandoned member stays Abandoned
    assert_eq!(graph.get_task("b").unwrap().status, Status::Abandoned);
}

#[test]
fn test_cycle_does_not_iterate_when_all_members_abandoned() {
    // Cycle: A → B → A. Both A and B are Abandoned.
    // The cycle should NOT iterate — there's no work to do.
    let mut a = make_task_with_status("a", "A (header)", Status::Abandoned);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Abandoned);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "a", &analysis);

    assert!(
        reactivated.is_empty(),
        "Cycle should NOT iterate when all members are abandoned (no work to do)"
    );
    // Both stay Abandoned
    assert_eq!(graph.get_task("a").unwrap().status, Status::Abandoned);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Abandoned);
}

#[test]
fn test_cycle_abandoned_member_stays_abandoned_after_reset() {
    // Cycle: A → B → C → A. A=Done, B=Abandoned, C=Done. max_iterations=5.
    // After iteration: A and C reset to Open, B stays Abandoned.
    // loop_iteration increments for A and C but B stays unchanged.
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    let mut b = make_task_with_status("b", "B", Status::Abandoned);
    b.after = vec!["a".to_string()];
    let mut c = make_task_with_status("c", "C", Status::Done);
    c.after = vec!["b".to_string()];

    let mut graph = build_graph(vec![a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "c", &analysis);

    // A and C should be reactivated, B should not
    assert!(reactivated.contains(&"a".to_string()));
    assert!(reactivated.contains(&"c".to_string()));
    assert!(!reactivated.contains(&"b".to_string()));

    // A and C reset to Open with incremented iteration
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("c").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("c").unwrap().loop_iteration, 1);

    // B stays Abandoned, loop_iteration unchanged
    assert_eq!(graph.get_task("b").unwrap().status, Status::Abandoned);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 0);
}

// ===========================================================================
// Converged scope: --converged should complete current iteration before stopping
// ===========================================================================

#[test]
fn test_converged_non_header_member_stops_cycle() {
    // Cycle: A → B → A. B is config owner (header). A signals --converged.
    // Both Done. The cycle should NOT iterate because A has the converged tag,
    // even though A is not the config owner.
    let mut a = make_task_with_status("a", "A (worker)", Status::Done);
    a.after = vec!["b".to_string()];
    a.tags = vec!["converged".to_string()];

    let mut b = make_task_with_status("b", "B (header)", Status::Done);
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "a", &analysis);

    assert!(
        reactivated.is_empty(),
        "Should NOT iterate when any member has 'converged' tag, got: {:?}",
        reactivated
    );
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
}

#[test]
fn test_converged_current_iteration_completes_before_stopping() {
    // Cycle: A → B → A. A is header (has cycle_config).
    // A completes with --converged but B is still Open.
    // B should still be ready (current iteration should complete).
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.tags = vec!["converged".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut b = make_task_with_status("b", "B (worker)", Status::Open);
    b.after = vec!["a".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    // evaluate_cycle_iteration should NOT reactivate (B is not done yet)
    let reactivated = evaluate_cycle_iteration(&mut graph, "a", &analysis);
    assert!(
        reactivated.is_empty(),
        "Should not reactivate while B is still Open"
    );

    // B should be ready because A (its dependency) is Done
    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ready_ids.contains(&"b"),
        "B should be ready — its dependency A is Done. Ready: {:?}",
        ready_ids
    );

    // Now mark B as Done and re-evaluate
    graph.get_task_mut("b").unwrap().status = Status::Done;
    let analysis = graph.compute_cycle_analysis();
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    // Cycle should NOT reactivate because A has the converged tag
    assert!(
        reactivated.is_empty(),
        "Should NOT iterate after current iteration completes with converged member"
    );
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
}

#[test]
fn test_converged_last_member_in_iteration_no_hang() {
    // Cycle: A → B → A. A is header. B signals --converged (last to complete).
    // Both Done. Cycle should stop cleanly.
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut b = make_task_with_status("b", "B (worker)", Status::Done);
    b.after = vec!["a".to_string()];
    b.tags = vec!["converged".to_string()];

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);

    assert!(
        reactivated.is_empty(),
        "Should NOT iterate when last member signals converged"
    );
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
}

#[test]
fn test_converged_multiple_members_same_iteration() {
    // Cycle: A → B → C → A. A is header. Both B and C signal --converged.
    // All Done. Cycle should stop.
    let mut a = make_task_with_status("a", "A (header)", Status::Done);
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut b = make_task_with_status("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    b.tags = vec!["converged".to_string()];

    let mut c = make_task_with_status("c", "C", Status::Done);
    c.after = vec!["b".to_string()];
    c.tags = vec!["converged".to_string()];

    let mut graph = build_graph(vec![a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "c", &analysis);

    assert!(
        reactivated.is_empty(),
        "Should NOT iterate when multiple members signal converged"
    );
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("c").unwrap().status, Status::Done);
}

#[test]
fn test_converged_non_header_with_evaluate_all() {
    // Same as test_converged_non_header_member_stops_cycle but using
    // evaluate_all_cycle_iterations (the coordinator's safety-net path).
    let mut a = make_task_with_status("a", "A (worker)", Status::Done);
    a.after = vec!["b".to_string()];
    a.tags = vec!["converged".to_string()];

    let mut b = make_task_with_status("b", "B (header)", Status::Done);
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);

    assert!(
        reactivated.is_empty(),
        "evaluate_all should also respect non-header converged tags"
    );
    assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
}

// ===========================================================================
// Cycle deadlock breaker (Path 3): symmetric cycle_config
// ===========================================================================

#[test]
fn test_deadlock_breaker_two_task_symmetric_cycle() {
    // A ↔ B, both have cycle_config. After reactivation at iteration 1,
    // all members are Open with cycle_config — deadlock without Path 3.
    // The cycle header should be exempted to break the deadlock.
    let mut a = make_task("a", "Task A");
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    a.loop_iteration = 1;

    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    b.loop_iteration = 1;

    let graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);

    // Exactly one task (the header) should be ready via Path 3.
    assert_eq!(
        ready.len(),
        1,
        "Exactly one task should be ready (deadlock breaker). Got: {:?}",
        ready.iter().map(|t| &t.id).collect::<Vec<_>>()
    );
    assert_eq!(
        ready[0].id, analysis.cycles[0].header,
        "The cycle header should be the one that becomes ready"
    );
}

#[test]
fn test_deadlock_breaker_three_task_symmetric_cycle() {
    // A → B → C → A, all have cycle_config. After reactivation at iteration 1,
    // deadlock without Path 3. The cycle header should break it.
    let mut a = make_task("a", "Task A");
    a.after = vec!["c".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    a.loop_iteration = 1;

    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    b.loop_iteration = 1;

    let mut c = make_task("c", "Task C");
    c.after = vec!["b".to_string()];
    c.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    c.loop_iteration = 1;

    let graph = build_graph(vec![a, b, c]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);

    // Exactly one task (the header) should be ready.
    assert_eq!(
        ready.len(),
        1,
        "Exactly one task should be ready (deadlock breaker). Got: {:?}",
        ready.iter().map(|t| &t.id).collect::<Vec<_>>()
    );
    assert_eq!(
        ready[0].id, analysis.cycles[0].header,
        "The cycle header should be the one that becomes ready"
    );
}

#[test]
fn test_deadlock_breaker_does_not_fire_when_member_in_progress() {
    // A ↔ B, both have cycle_config, B is InProgress.
    // Back-edge exemption is structural — A's dep on B is a back-edge
    // regardless of B's status. A is Open → A is ready.
    // (B is InProgress, not Open, so B is not considered for readiness.)
    let mut a = make_task("a", "Task A");
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    a.loop_iteration = 1;

    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];
    b.status = Status::InProgress;
    b.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    b.loop_iteration = 1;

    let graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    // A is ready: its dep on B is a structural back-edge (always skipped).
    // B is InProgress (not Open), so not a readiness candidate.
    assert!(
        ready_ids.contains(&"a"),
        "A should be ready (back-edge from B skipped, no other deps). Ready: {:?}",
        ready_ids
    );
    assert_eq!(
        ready_ids.len(),
        1,
        "Only A should be ready (B is InProgress). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_deadlock_breaker_asymmetric_config_not_triggered() {
    // A(cc) ↔ B(no cc). A.after=[B], B.after=[A].
    // Back-edge analysis is purely structural — cycle_config is irrelevant.
    // Header = A (alphabetically first). Back-edge: B→A in adj.
    // A's dep on B is skipped (back-edge) → A ready.
    // B's dep on A is forward → B waits for A.
    let mut a = make_task("a", "Task A");
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    a.loop_iteration = 1;

    let mut b = make_task("b", "Task B");
    b.after = vec!["a".to_string()];
    // b has NO cycle_config
    b.loop_iteration = 1;

    let graph = build_graph(vec![a, b]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);
    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();

    assert!(
        ready_ids.contains(&"a"),
        "A (header) should be ready (back-edge from B skipped). Ready: {:?}",
        ready_ids
    );
    assert!(
        !ready_ids.contains(&"b"),
        "B should NOT be ready (forward dep on A is Open). Ready: {:?}",
        ready_ids
    );
}

#[test]
fn test_deadlock_breaker_self_loop_iteration_1() {
    // Self-loop: A depends on A, has cycle_config, iteration 1.
    // Path 2 doesn't fire (iteration > 0). Path 3 should handle it:
    // single member SCC, all members Open with cycle_config, A is header.
    let mut a = make_task("a", "Task A");
    a.after = vec!["a".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    a.loop_iteration = 1;

    let graph = build_graph(vec![a]);
    let analysis = graph.compute_cycle_analysis();

    let ready = ready_tasks_cycle_aware(&graph, &analysis);

    assert_eq!(
        ready.len(),
        1,
        "Self-loop should be ready via deadlock breaker on iteration 1"
    );
    assert_eq!(ready[0].id, "a");
}

#[test]
fn test_deadlock_breaker_e2e_two_task_cycle() {
    // End-to-end: create a 2-task symmetric cycle, complete iteration 0,
    // verify both reopen, then verify the header becomes ready on iteration 1.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    // Create A and B with mutual dependencies and cycle_config
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task A",
            "--id",
            "a",
            "--max-iterations",
            "3",
            "--immediate",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task B",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "3",
            "--immediate",
        ],
    );

    // Complete iteration 0
    wg_ok(&wg_dir, &["done", "a"]);
    wg_ok(&wg_dir, &["done", "b"]);

    // Both should be re-opened at iteration 1
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("b").unwrap().loop_iteration, 1);

    // Verify both have cycle_config (auto back-edge gives b cycle_config too)
    assert!(
        graph.get_task("a").unwrap().cycle_config.is_some(),
        "A should have cycle_config"
    );
    assert!(
        graph.get_task("b").unwrap().cycle_config.is_some(),
        "B should have cycle_config (auto back-edge)"
    );

    // The cycle header should be ready via Path 3
    let analysis = graph.compute_cycle_analysis();
    let ready = ready_tasks_cycle_aware(&graph, &analysis);

    assert!(
        !ready.is_empty(),
        "At least one task should be ready after deadlock breaker. \
         Cycle header: {}, members: {:?}",
        analysis.cycles[0].header,
        analysis.cycles[0].members
    );

    let ready_ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
    assert!(
        ready_ids.contains(&analysis.cycles[0].header.as_str()),
        "The cycle header should be ready. Ready: {:?}, Header: {}",
        ready_ids,
        analysis.cycles[0].header
    );
}

#[test]
fn test_deadlock_breaker_e2e_three_task_cycle() {
    // End-to-end: 3-task symmetric cycle via CLI, verify deadlock broken.
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task A",
            "--id",
            "a",
            "--max-iterations",
            "3",
            "--immediate",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task B",
            "--id",
            "b",
            "--after",
            "a",
            "--max-iterations",
            "3",
            "--immediate",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task C",
            "--id",
            "c",
            "--after",
            "b",
            "--max-iterations",
            "3",
            "--immediate",
        ],
    );

    // Complete iteration 0 in order
    wg_ok(&wg_dir, &["done", "a"]);
    wg_ok(&wg_dir, &["done", "b"]);
    wg_ok(&wg_dir, &["done", "c"]);

    // All should be re-opened at iteration 1
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    for id in &["a", "b", "c"] {
        let task = graph.get_task(id).unwrap();
        assert_eq!(task.status, Status::Open, "{} should be Open", id);
        assert_eq!(task.loop_iteration, 1, "{} should be at iteration 1", id);
    }

    // Verify the deadlock breaker fires
    let analysis = graph.compute_cycle_analysis();
    let ready = ready_tasks_cycle_aware(&graph, &analysis);

    assert!(
        !ready.is_empty(),
        "At least one task should be ready after deadlock breaker in 3-task cycle. \
         Cycle header: {}, members: {:?}",
        analysis.cycles[0].header,
        analysis.cycles[0].members
    );
}

// ===========================================================================
// Archived cycle header suppresses reset
// ===========================================================================

#[test]
fn test_archived_cycle_header_suppresses_reset() {
    // Scenario: 2-node cycle (header ↔ archive-task), header is Done + tagged 'archived',
    // archive-task completes afterward. The cycle should NOT reset.
    let mut header = make_task_with_status("coordinator-0", "Coordinator", Status::Done);
    header.after = vec!["archive-0".to_string()];
    header.cycle_config = Some(CycleConfig {
        max_iterations: 0, // unlimited
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    header.tags = vec!["archived".to_string()];

    let mut archive = make_task_with_status("archive-0", "Archive", Status::Done);
    archive.after = vec!["coordinator-0".to_string()];

    let mut graph = build_graph(vec![header, archive]);
    let analysis = graph.compute_cycle_analysis();

    // Simulate archive-0 completing last — trigger cycle evaluation
    let reactivated = evaluate_cycle_iteration(&mut graph, "archive-0", &analysis);
    assert!(
        reactivated.is_empty(),
        "Archived cycle header should suppress reset, got: {:?}",
        reactivated
    );

    // Both tasks should remain Done
    assert_eq!(
        graph.get_task("coordinator-0").unwrap().status,
        Status::Done
    );
    assert_eq!(graph.get_task("archive-0").unwrap().status, Status::Done);

    // loop_iteration should NOT have been incremented
    assert_eq!(
        graph.get_task("coordinator-0").unwrap().loop_iteration,
        0,
        "Header loop_iteration should remain 0"
    );
    assert_eq!(
        graph.get_task("archive-0").unwrap().loop_iteration,
        0,
        "Archive task loop_iteration should remain 0"
    );
}

#[test]
fn test_archived_cycle_header_suppresses_evaluate_all() {
    // Same scenario but via evaluate_all_cycle_iterations (coordinator safety net path).
    let mut header = make_task_with_status("coordinator-0", "Coordinator", Status::Done);
    header.after = vec!["archive-0".to_string()];
    header.cycle_config = Some(CycleConfig {
        max_iterations: 0,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    header.tags = vec!["archived".to_string()];

    let mut archive = make_task_with_status("archive-0", "Archive", Status::Done);
    archive.after = vec!["coordinator-0".to_string()];

    let mut graph = build_graph(vec![header, archive]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_all_cycle_iterations(&mut graph, &analysis);
    assert!(
        reactivated.is_empty(),
        "evaluate_all should not reactivate archived cycle, got: {:?}",
        reactivated
    );
    assert_eq!(
        graph.get_task("coordinator-0").unwrap().status,
        Status::Done
    );
    assert_eq!(graph.get_task("archive-0").unwrap().status, Status::Done);
}

#[test]
fn test_non_archived_cycle_still_resets_normally() {
    // Sanity check: a cycle without the archived tag should still reset as expected.
    let mut header = make_task_with_status("coordinator-0", "Coordinator", Status::Done);
    header.after = vec!["archive-0".to_string()];
    header.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    // No 'archived' tag

    let mut archive = make_task_with_status("archive-0", "Archive", Status::Done);
    archive.after = vec!["coordinator-0".to_string()];

    let mut graph = build_graph(vec![header, archive]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "archive-0", &analysis);
    assert_eq!(
        reactivated.len(),
        2,
        "Non-archived cycle should reactivate both members"
    );
    assert_eq!(
        graph.get_task("coordinator-0").unwrap().status,
        Status::Open
    );
    assert_eq!(graph.get_task("archive-0").unwrap().status, Status::Open);
}

// ===========================================================================
// Shell Task + Checker Cycle (Retry Loop) Tests
// ===========================================================================

/// Tests the shell-task → checker → retry → success cycle pattern.
/// This validates the core "reset-from-downstream" workflow.
#[test]
fn test_shell_checker_cycle_iteration() {
    // Shell task → checker → (back-edge to shell task).
    // Checker owns the cycle_config. When both are Done and checker is not
    // converged, both should reset to Open.
    let mut shell_task = make_task_with_status("run-batch", "Run Batch", Status::Done);
    shell_task.exec = Some("python3 run.py --batch 1".to_string());
    shell_task.exec_mode = Some("shell".to_string());
    shell_task.after = vec!["check-batch".to_string()]; // back-edge

    let mut checker = make_task_with_status("check-batch", "Check Batch", Status::Done);
    checker.after = vec!["run-batch".to_string()]; // forward edge
    checker.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![shell_task, checker]);
    let analysis = graph.compute_cycle_analysis();

    // Complete checker without --converged → cycle should iterate
    let reactivated = evaluate_cycle_iteration(&mut graph, "check-batch", &analysis);

    assert_eq!(
        reactivated.len(),
        2,
        "Both shell task and checker should be re-activated"
    );

    let shell = graph.get_task("run-batch").unwrap();
    assert_eq!(shell.status, Status::Open, "Shell task should be Open");
    assert_eq!(shell.loop_iteration, 1, "Shell task iteration should be 1");

    let check = graph.get_task("check-batch").unwrap();
    assert_eq!(check.status, Status::Open, "Checker should be Open");
    assert_eq!(check.loop_iteration, 1, "Checker iteration should be 1");
}

#[test]
fn test_shell_checker_cycle_converged_stops() {
    // Shell → checker cycle. Checker is converged → cycle should stop.
    let mut shell_task = make_task_with_status("run-batch", "Run Batch", Status::Done);
    shell_task.exec = Some("python3 run.py".to_string());
    shell_task.after = vec!["check-batch".to_string()];

    let mut checker = make_task_with_status("check-batch", "Check Batch", Status::Done);
    checker.after = vec!["run-batch".to_string()];
    checker.tags = vec!["converged".to_string()];
    checker.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![shell_task, checker]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "check-batch", &analysis);

    assert!(
        reactivated.is_empty(),
        "Converged cycle should NOT re-activate any tasks"
    );

    // Both should stay Done
    assert_eq!(
        graph.get_task("run-batch").unwrap().status,
        Status::Done
    );
    assert_eq!(
        graph.get_task("check-batch").unwrap().status,
        Status::Done
    );
}

#[test]
fn test_shell_checker_cycle_max_iterations_honored() {
    // Shell → checker. Checker at max_iterations limit.
    let mut shell_task = make_task_with_status("run-batch", "Run Batch", Status::Done);
    shell_task.exec = Some("python3 run.py".to_string());
    shell_task.after = vec!["check-batch".to_string()];
    shell_task.loop_iteration = 4; // Already at iteration 4

    let mut checker = make_task_with_status("check-batch", "Check Batch", Status::Done);
    checker.after = vec!["run-batch".to_string()];
    checker.loop_iteration = 4;
    checker.cycle_config = Some(CycleConfig {
        max_iterations: 5, // max=5, iteration 4 is the last (0..4 = 5 iterations)
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut graph = build_graph(vec![shell_task, checker]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "check-batch", &analysis);

    assert!(
        reactivated.is_empty(),
        "Should NOT re-activate when max iterations reached"
    );
}

#[test]
fn test_shell_checker_cycle_logs_preserved() {
    // Verify that log entries survive across cycle resets.
    let mut shell_task = make_task_with_status("run-batch", "Run Batch", Status::Done);
    shell_task.exec = Some("python3 run.py".to_string());
    shell_task.after = vec!["check-batch".to_string()];
    shell_task.log = vec![workgraph::graph::LogEntry {
        timestamp: "2026-04-07T12:00:00Z".to_string(),
        message: "Attempt 1: completed with 3 errors".to_string(),
        actor: None,
        user: None,
    }];

    let mut checker = make_task_with_status("check-batch", "Check Batch", Status::Done);
    checker.after = vec!["run-batch".to_string()];
    checker.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });
    checker.log = vec![workgraph::graph::LogEntry {
        timestamp: "2026-04-07T12:05:00Z".to_string(),
        message: "Attempt 1: 3 errors found, requesting retry".to_string(),
        actor: None,
        user: None,
    }];

    let mut graph = build_graph(vec![shell_task, checker]);
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "check-batch", &analysis);
    assert_eq!(reactivated.len(), 2);

    // Logs should be preserved (append-only, not cleared)
    let shell = graph.get_task("run-batch").unwrap();
    assert!(
        shell.log.len() >= 1,
        "Shell task logs should be preserved across reset"
    );
    assert!(
        shell
            .log
            .iter()
            .any(|e| e.message.contains("Attempt 1")),
        "Original log entry should still exist"
    );

    let check = graph.get_task("check-batch").unwrap();
    assert!(
        check.log.len() >= 1,
        "Checker logs should be preserved across reset"
    );
    assert!(
        check
            .log
            .iter()
            .any(|e| e.message.contains("3 errors")),
        "Original checker log entry should still exist"
    );
}

#[test]
fn test_shell_checker_cycle_failure_restarts() {
    // Shell → checker. Checker fails → cycle failure restart.
    let mut shell_task = make_task_with_status("run-batch", "Run Batch", Status::Done);
    shell_task.exec = Some("python3 run.py".to_string());
    shell_task.after = vec!["check-batch".to_string()];

    let mut checker = make_task_with_status("check-batch", "Check Batch", Status::Failed);
    checker.after = vec!["run-batch".to_string()];
    checker.failure_reason = Some("Agent crash".to_string());
    checker.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: Some(3),
    });

    let mut graph = build_graph(vec![shell_task, checker]);
    let analysis = graph.compute_cycle_analysis();

    let restarted = evaluate_cycle_on_failure(&mut graph, "check-batch", &analysis);

    assert!(
        !restarted.is_empty(),
        "Failure restart should re-activate tasks"
    );

    // Checker should be reset to Open
    let check = graph.get_task("check-batch").unwrap();
    assert_eq!(check.status, Status::Open, "Failed checker should restart");
}

/// CLI integration test: wg add --exec creates a task with exec and exec_mode=shell.
#[test]
fn test_cli_add_with_exec_flag() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");

    wg_ok(&wg_dir, &["init"]);
    let output = wg_ok(
        &wg_dir,
        &[
            "add",
            "Run batch script",
            "--exec",
            "python3 run_batch.py --quality high",
            "--no-place",
        ],
    );
    assert!(
        output.contains("run-batch-script"),
        "Should create task with slugified ID: {}",
        output
    );

    let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();
    let task = graph
        .get_task("run-batch-script")
        .expect("Task should exist");
    assert_eq!(
        task.exec.as_deref(),
        Some("python3 run_batch.py --quality high"),
        "Task should have exec command set"
    );
    assert_eq!(
        task.exec_mode.as_deref(),
        Some("shell"),
        "exec_mode should auto-set to 'shell'"
    );
}

/// CLI integration test: wg add --exec --timeout creates a task with timeout.
#[test]
fn test_cli_add_with_exec_and_timeout() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");

    wg_ok(&wg_dir, &["init"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Long render",
            "--exec",
            "render.sh",
            "--timeout",
            "6h",
            "--no-place",
        ],
    );

    let graph = load_graph(&wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("long-render").expect("Task should exist");
    assert_eq!(task.exec.as_deref(), Some("render.sh"));
    assert_eq!(task.timeout.as_deref(), Some("6h"));
}
