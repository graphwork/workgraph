//! Integration tests for loop edge behavior.
//!
//! These tests exercise the actual graph/done/loop machinery in `evaluate_loop_edges`,
//! `find_intermediate_tasks`, `ready_tasks`, and `is_time_ready` — no mocks.

use chrono::{Duration, Utc};
use workgraph::graph::{LoopEdge, LoopGuard, Node, Status, Task, WorkGraph, evaluate_loop_edges};
use workgraph::query::{is_time_ready, ready_tasks};

/// Helper: create a minimal open task.
fn make_task(id: &str) -> Task {
    Task {
        id: id.to_string(),
        title: format!("Task {}", id),
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
        loops_to: vec![],
        loop_iteration: 0,
        ready_after: None,
        paused: false,
    }
}

// ---------------------------------------------------------------------------
// Test 1: Basic loop — A→B→C where C loops_to A
// ---------------------------------------------------------------------------
#[test]
fn test_basic_loop_a_b_c_loops_to_a() {
    let mut graph = WorkGraph::new();

    let a = make_task("a");
    let mut b = make_task("b");
    b.blocked_by = vec!["a".to_string()];
    let mut c = make_task("c");
    c.blocked_by = vec!["b".to_string()];
    c.loops_to = vec![LoopEdge {
        target: "a".to_string(),
        guard: None,
        max_iterations: 5,
        delay: None,
    }];

    graph.add_node(Node::Task(a));
    graph.add_node(Node::Task(b));
    graph.add_node(Node::Task(c));

    // Simulate completing all three tasks in order
    graph.get_task_mut("a").unwrap().status = Status::Done;
    graph.get_task_mut("b").unwrap().status = Status::Done;
    graph.get_task_mut("c").unwrap().status = Status::Done;

    // Fire loop edges from C
    let reactivated = evaluate_loop_edges(&mut graph, "c");

    // A should be re-opened with loop_iteration incremented
    let a = graph.get_task("a").unwrap();
    assert_eq!(a.status, Status::Open, "A should be re-opened");
    assert_eq!(a.loop_iteration, 1, "A's loop_iteration should be 1");

    // B and C should be re-opened as intermediates (they depend on A transitively)
    let b = graph.get_task("b").unwrap();
    assert_eq!(
        b.status,
        Status::Open,
        "B should be re-opened via propagation"
    );

    // reactivated should contain a, b, and c (source is also re-opened)
    assert!(reactivated.contains(&"a".to_string()));
    assert!(reactivated.contains(&"b".to_string()));
    assert!(reactivated.contains(&"c".to_string()));

    let c = graph.get_task("c").unwrap();
    assert_eq!(c.status, Status::Open, "C (source) should be re-opened");
    assert_eq!(c.loop_iteration, 1, "C's loop_iteration should be 1");
}

// ---------------------------------------------------------------------------
// Test 2: Self-loop — task loops_to itself
// ---------------------------------------------------------------------------
#[test]
fn test_self_loop() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("self");
    t.loops_to = vec![LoopEdge {
        target: "self".to_string(),
        guard: None,
        max_iterations: 10,
        delay: None,
    }];
    graph.add_node(Node::Task(t));

    // Complete the task
    graph.get_task_mut("self").unwrap().status = Status::Done;

    let reactivated = evaluate_loop_edges(&mut graph, "self");

    let t = graph.get_task("self").unwrap();
    assert_eq!(t.status, Status::Open, "Self-loop task should be re-opened");
    assert_eq!(t.loop_iteration, 1, "Self-loop iteration should be 1");
    assert!(reactivated.contains(&"self".to_string()));
}

// ---------------------------------------------------------------------------
// Test 3: Self-loop with delay — verify ready_after is set
// ---------------------------------------------------------------------------
#[test]
fn test_self_loop_with_delay() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("delayed");
    t.loops_to = vec![LoopEdge {
        target: "delayed".to_string(),
        guard: None,
        max_iterations: 10,
        delay: Some("30s".to_string()),
    }];
    graph.add_node(Node::Task(t));

    // Complete the task
    graph.get_task_mut("delayed").unwrap().status = Status::Done;

    let reactivated = evaluate_loop_edges(&mut graph, "delayed");
    assert!(reactivated.contains(&"delayed".to_string()));

    let t = graph.get_task("delayed").unwrap();
    assert_eq!(t.status, Status::Open, "Task should be re-opened");
    assert_eq!(t.loop_iteration, 1);

    // ready_after should be set to ~30 seconds from now
    assert!(
        t.ready_after.is_some(),
        "ready_after should be set for delayed loop"
    );
    let ready_after: chrono::DateTime<Utc> = t.ready_after.as_ref().unwrap().parse().unwrap();
    let now = Utc::now();
    // Should be roughly 30 seconds from now (allow 5s tolerance for test execution)
    assert!(
        ready_after > now + Duration::seconds(25),
        "ready_after should be at least 25s from now"
    );
    assert!(
        ready_after < now + Duration::seconds(35),
        "ready_after should be at most 35s from now"
    );

    // Task should be Open but NOT time-ready (ready_after is in the future)
    assert!(
        !is_time_ready(t),
        "Task with future ready_after should not be time-ready"
    );

    // ready_tasks should not include it
    let ready = ready_tasks(&graph);
    assert!(
        !ready.iter().any(|r| r.id == "delayed"),
        "Delayed task should not appear in ready_tasks"
    );
}

// ---------------------------------------------------------------------------
// Test 4: Guard condition — loop fires only when guard is true
// ---------------------------------------------------------------------------
#[test]
fn test_guard_condition_true_fires() {
    let mut graph = WorkGraph::new();

    // Guard task that controls the loop
    let mut gate = make_task("gate");
    gate.status = Status::Done; // gate is Done, so TaskStatus guard will match

    let mut worker = make_task("worker");
    worker.loops_to = vec![LoopEdge {
        target: "worker".to_string(),
        guard: Some(LoopGuard::TaskStatus {
            task: "gate".to_string(),
            status: Status::Done,
        }),
        max_iterations: 5,
        delay: None,
    }];

    graph.add_node(Node::Task(gate));
    graph.add_node(Node::Task(worker));

    // Complete worker
    graph.get_task_mut("worker").unwrap().status = Status::Done;
    let reactivated = evaluate_loop_edges(&mut graph, "worker");

    // Guard is true (gate is Done) → loop should fire
    let w = graph.get_task("worker").unwrap();
    assert_eq!(
        w.status,
        Status::Open,
        "Loop should fire when guard is true"
    );
    assert_eq!(w.loop_iteration, 1);
    assert!(reactivated.contains(&"worker".to_string()));
}

#[test]
fn test_guard_condition_false_does_not_fire() {
    let mut graph = WorkGraph::new();

    // Guard task is NOT Done → guard should fail
    let gate = make_task("gate"); // Status::Open

    let mut worker = make_task("worker");
    worker.loops_to = vec![LoopEdge {
        target: "worker".to_string(),
        guard: Some(LoopGuard::TaskStatus {
            task: "gate".to_string(),
            status: Status::Done,
        }),
        max_iterations: 5,
        delay: None,
    }];

    graph.add_node(Node::Task(gate));
    graph.add_node(Node::Task(worker));

    // Complete worker
    graph.get_task_mut("worker").unwrap().status = Status::Done;
    let reactivated = evaluate_loop_edges(&mut graph, "worker");

    // Guard is false (gate is Open, not Done) → loop should NOT fire
    let w = graph.get_task("worker").unwrap();
    assert_eq!(
        w.status,
        Status::Done,
        "Loop should NOT fire when guard is false"
    );
    assert_eq!(w.loop_iteration, 0);
    assert!(reactivated.is_empty());
}

// ---------------------------------------------------------------------------
// Test 5: Max iterations — loop fires up to max_iterations then stops
// ---------------------------------------------------------------------------
#[test]
fn test_max_iterations() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("bounded");
    t.loops_to = vec![LoopEdge {
        target: "bounded".to_string(),
        guard: None,
        max_iterations: 3,
        delay: None,
    }];
    graph.add_node(Node::Task(t));

    // Loop 3 times: iteration goes 0→1, 1→2, 2→3
    for expected_iter in 1..=3 {
        graph.get_task_mut("bounded").unwrap().status = Status::Done;
        let reactivated = evaluate_loop_edges(&mut graph, "bounded");

        if expected_iter <= 3 {
            let t = graph.get_task("bounded").unwrap();
            if expected_iter < 3 {
                assert_eq!(
                    t.status,
                    Status::Open,
                    "Should re-open at iteration {}",
                    expected_iter
                );
                assert_eq!(t.loop_iteration, expected_iter);
                assert!(reactivated.contains(&"bounded".to_string()));
            }
        }
    }

    // After 3 iterations, loop_iteration == 3 == max_iterations.
    // Complete again: loop should NOT fire since iteration >= max
    graph.get_task_mut("bounded").unwrap().status = Status::Done;
    let reactivated = evaluate_loop_edges(&mut graph, "bounded");

    let t = graph.get_task("bounded").unwrap();
    assert_eq!(
        t.status,
        Status::Done,
        "Task should stay Done when max_iterations reached"
    );
    assert_eq!(t.loop_iteration, 3);
    assert!(
        reactivated.is_empty(),
        "No reactivation past max_iterations"
    );
}

// ---------------------------------------------------------------------------
// Test 6: Unbounded loop (high max_iterations) — verify it keeps looping
// ---------------------------------------------------------------------------
#[test]
fn test_unbounded_loop_keeps_looping() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("unbounded");
    // Use a very large max_iterations to simulate "unbounded"
    t.loops_to = vec![LoopEdge {
        target: "unbounded".to_string(),
        guard: None,
        max_iterations: u32::MAX,
        delay: None,
    }];
    graph.add_node(Node::Task(t));

    // Run 20 iterations — well beyond any small fixed count
    for i in 1..=20 {
        graph.get_task_mut("unbounded").unwrap().status = Status::Done;
        let reactivated = evaluate_loop_edges(&mut graph, "unbounded");

        let t = graph.get_task("unbounded").unwrap();
        assert_eq!(t.status, Status::Open, "Should re-open at iteration {}", i);
        assert_eq!(t.loop_iteration, i);
        assert!(reactivated.contains(&"unbounded".to_string()));
    }
}

// ---------------------------------------------------------------------------
// Test 7: Multi-step propagation — A→B→C→D where D loops_to A
// ---------------------------------------------------------------------------
#[test]
fn test_multi_step_propagation() {
    let mut graph = WorkGraph::new();

    let a = make_task("a");
    let mut b = make_task("b");
    b.blocked_by = vec!["a".to_string()];
    let mut c = make_task("c");
    c.blocked_by = vec!["b".to_string()];
    let mut d = make_task("d");
    d.blocked_by = vec!["c".to_string()];
    d.loops_to = vec![LoopEdge {
        target: "a".to_string(),
        guard: None,
        max_iterations: 5,
        delay: None,
    }];

    graph.add_node(Node::Task(a));
    graph.add_node(Node::Task(b));
    graph.add_node(Node::Task(c));
    graph.add_node(Node::Task(d));

    // Complete all four tasks
    for id in &["a", "b", "c", "d"] {
        graph.get_task_mut(id).unwrap().status = Status::Done;
    }

    let reactivated = evaluate_loop_edges(&mut graph, "d");

    // A should be re-opened
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("a").unwrap().loop_iteration, 1);

    // B and C are intermediate tasks that were Done and should be re-opened
    assert_eq!(
        graph.get_task("b").unwrap().status,
        Status::Open,
        "B should be re-opened as intermediate"
    );
    assert_eq!(
        graph.get_task("c").unwrap().status,
        Status::Open,
        "C should be re-opened as intermediate"
    );

    // D is the source — it should be re-opened as part of the cycle
    assert_eq!(
        graph.get_task("d").unwrap().status,
        Status::Open,
        "D (source) should be re-opened by loop"
    );
    assert_eq!(graph.get_task("d").unwrap().loop_iteration, 1);

    // All of a, b, c, d should be in reactivated
    assert!(reactivated.contains(&"a".to_string()));
    assert!(reactivated.contains(&"b".to_string()));
    assert!(reactivated.contains(&"c".to_string()));
    assert!(reactivated.contains(&"d".to_string()));
}

// ---------------------------------------------------------------------------
// Test 8: Loop with artifacts — verify artifacts survive loop re-activation
// ---------------------------------------------------------------------------
#[test]
fn test_loop_with_artifacts() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("artsy");
    t.loops_to = vec![LoopEdge {
        target: "artsy".to_string(),
        guard: None,
        max_iterations: 5,
        delay: None,
    }];
    graph.add_node(Node::Task(t));

    // Add artifacts before completing
    {
        let t = graph.get_task_mut("artsy").unwrap();
        t.artifacts = vec![
            "output/report-v1.pdf".to_string(),
            "output/data-v1.csv".to_string(),
        ];
        t.status = Status::Done;
    }

    let reactivated = evaluate_loop_edges(&mut graph, "artsy");
    assert!(reactivated.contains(&"artsy".to_string()));

    // After re-activation, artifacts from iteration 0 should still be present
    let t = graph.get_task("artsy").unwrap();
    assert_eq!(t.status, Status::Open);
    assert_eq!(t.loop_iteration, 1);
    assert_eq!(
        t.artifacts.len(),
        2,
        "Artifacts from previous iteration should still be accessible"
    );
    assert!(t.artifacts.contains(&"output/report-v1.pdf".to_string()));
    assert!(t.artifacts.contains(&"output/data-v1.csv".to_string()));
}

// ---------------------------------------------------------------------------
// Test 9: Loop iteration counter — verify it increments correctly across cycles
// ---------------------------------------------------------------------------
#[test]
fn test_loop_iteration_counter_increments() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("counter");
    t.loops_to = vec![LoopEdge {
        target: "counter".to_string(),
        guard: None,
        max_iterations: 10,
        delay: None,
    }];
    graph.add_node(Node::Task(t));

    assert_eq!(graph.get_task("counter").unwrap().loop_iteration, 0);

    for expected in 1..=7 {
        graph.get_task_mut("counter").unwrap().status = Status::Done;
        evaluate_loop_edges(&mut graph, "counter");

        let t = graph.get_task("counter").unwrap();
        assert_eq!(
            t.loop_iteration, expected,
            "After cycle {}, loop_iteration should be {}",
            expected, expected
        );
        assert_eq!(t.status, Status::Open);
    }
}

// ---------------------------------------------------------------------------
// Test 10: Concurrent loops — two independent loops in the same graph
// ---------------------------------------------------------------------------
#[test]
fn test_concurrent_loops_no_interference() {
    let mut graph = WorkGraph::new();

    // Loop 1: alpha self-loops
    let mut alpha = make_task("alpha");
    alpha.loops_to = vec![LoopEdge {
        target: "alpha".to_string(),
        guard: None,
        max_iterations: 5,
        delay: None,
    }];

    // Loop 2: beta→gamma where gamma loops_to beta
    let beta = make_task("beta");
    let mut gamma = make_task("gamma");
    gamma.blocked_by = vec!["beta".to_string()];
    gamma.loops_to = vec![LoopEdge {
        target: "beta".to_string(),
        guard: None,
        max_iterations: 3,
        delay: None,
    }];

    graph.add_node(Node::Task(alpha));
    graph.add_node(Node::Task(beta));
    graph.add_node(Node::Task(gamma));

    // Complete alpha, fire its loop
    graph.get_task_mut("alpha").unwrap().status = Status::Done;
    let reactivated_alpha = evaluate_loop_edges(&mut graph, "alpha");

    assert!(reactivated_alpha.contains(&"alpha".to_string()));
    assert_eq!(graph.get_task("alpha").unwrap().loop_iteration, 1);

    // beta and gamma should be unaffected
    assert_eq!(graph.get_task("beta").unwrap().loop_iteration, 0);
    assert_eq!(graph.get_task("beta").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("gamma").unwrap().loop_iteration, 0);
    assert_eq!(graph.get_task("gamma").unwrap().status, Status::Open);

    // Now complete beta and gamma in loop 2
    graph.get_task_mut("beta").unwrap().status = Status::Done;
    graph.get_task_mut("gamma").unwrap().status = Status::Done;
    let reactivated_gamma = evaluate_loop_edges(&mut graph, "gamma");

    // beta should be re-opened by gamma's loop
    assert!(reactivated_gamma.contains(&"beta".to_string()));
    assert_eq!(graph.get_task("beta").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("beta").unwrap().status, Status::Open);

    // alpha should be unaffected by gamma's loop (still at iteration 1, Open from its own loop)
    assert_eq!(graph.get_task("alpha").unwrap().loop_iteration, 1);
    assert_eq!(graph.get_task("alpha").unwrap().status, Status::Open);

    // Complete alpha again — its iteration should advance to 2
    graph.get_task_mut("alpha").unwrap().status = Status::Done;
    evaluate_loop_edges(&mut graph, "alpha");
    assert_eq!(graph.get_task("alpha").unwrap().loop_iteration, 2);

    // beta still at iteration 1 — independent
    assert_eq!(graph.get_task("beta").unwrap().loop_iteration, 1);
}

// ---------------------------------------------------------------------------
// Bonus: IterationLessThan guard
// ---------------------------------------------------------------------------
#[test]
fn test_iteration_less_than_guard() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("guarded");
    t.loops_to = vec![LoopEdge {
        target: "guarded".to_string(),
        guard: Some(LoopGuard::IterationLessThan(2)),
        max_iterations: 100, // high cap, guard should stop it first
        delay: None,
    }];
    graph.add_node(Node::Task(t));

    // Iteration 0→1: guard says iteration < 2, current=0 so fires
    graph.get_task_mut("guarded").unwrap().status = Status::Done;
    let r = evaluate_loop_edges(&mut graph, "guarded");
    assert!(!r.is_empty());
    assert_eq!(graph.get_task("guarded").unwrap().loop_iteration, 1);

    // Iteration 1→2: guard says iteration < 2, current=1 so fires
    graph.get_task_mut("guarded").unwrap().status = Status::Done;
    let r = evaluate_loop_edges(&mut graph, "guarded");
    assert!(!r.is_empty());
    assert_eq!(graph.get_task("guarded").unwrap().loop_iteration, 2);

    // Iteration 2: guard says iteration < 2, current=2 so does NOT fire
    graph.get_task_mut("guarded").unwrap().status = Status::Done;
    let r = evaluate_loop_edges(&mut graph, "guarded");
    assert!(
        r.is_empty(),
        "IterationLessThan(2) should stop at iteration 2"
    );
    assert_eq!(graph.get_task("guarded").unwrap().status, Status::Done);
    assert_eq!(graph.get_task("guarded").unwrap().loop_iteration, 2);
}

// ---------------------------------------------------------------------------
// Bonus: Always guard
// ---------------------------------------------------------------------------
#[test]
fn test_always_guard_fires() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("always");
    t.loops_to = vec![LoopEdge {
        target: "always".to_string(),
        guard: Some(LoopGuard::Always),
        max_iterations: 3,
        delay: None,
    }];
    graph.add_node(Node::Task(t));

    graph.get_task_mut("always").unwrap().status = Status::Done;
    let r = evaluate_loop_edges(&mut graph, "always");
    assert!(r.contains(&"always".to_string()));
    assert_eq!(graph.get_task("always").unwrap().loop_iteration, 1);
}

// ---------------------------------------------------------------------------
// Test: Source task not re-opened when target doesn't exist
// ---------------------------------------------------------------------------
#[test]
fn test_loop_to_nonexistent_target_does_not_reopen_source() {
    let mut graph = WorkGraph::new();

    let mut src = make_task("src");
    src.loops_to = vec![LoopEdge {
        target: "nonexistent".to_string(),
        guard: None,
        max_iterations: 5,
        delay: None,
    }];
    graph.add_node(Node::Task(src));

    // Complete the source task
    graph.get_task_mut("src").unwrap().status = Status::Done;

    let reactivated = evaluate_loop_edges(&mut graph, "src");

    // Target doesn't exist, so nothing should be reactivated
    assert!(
        reactivated.is_empty(),
        "No tasks should be reactivated when target doesn't exist"
    );
    // Source should remain Done (not re-opened)
    let src = graph.get_task("src").unwrap();
    assert_eq!(
        src.status,
        Status::Done,
        "Source should stay Done when target doesn't exist"
    );
    assert_eq!(
        src.loop_iteration, 0,
        "Source loop_iteration should not increment"
    );
}

// ---------------------------------------------------------------------------
// Test: Source not re-opened when iteration limit already reached
// ---------------------------------------------------------------------------
#[test]
fn test_loop_source_not_reopened_when_at_max_iterations() {
    let mut graph = WorkGraph::new();

    let mut target = make_task("tgt");
    target.loop_iteration = 5; // Already at max
    let mut src = make_task("src");
    src.loops_to = vec![LoopEdge {
        target: "tgt".to_string(),
        guard: None,
        max_iterations: 5,
        delay: None,
    }];

    graph.add_node(Node::Task(target));
    graph.add_node(Node::Task(src));

    graph.get_task_mut("src").unwrap().status = Status::Done;
    let reactivated = evaluate_loop_edges(&mut graph, "src");

    assert!(
        reactivated.is_empty(),
        "No reactivation when target is at max iterations"
    );
    assert_eq!(
        graph.get_task("src").unwrap().status,
        Status::Done,
        "Source should remain Done when loop doesn't fire"
    );
}
