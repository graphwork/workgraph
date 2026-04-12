use workgraph::graph::{CycleAnalysis, Node, Status, WorkGraph};
use workgraph::query::ready_tasks_cycle_aware;

// Helper function to create a task
fn make_task(id: &str, title: &str) -> workgraph::graph::Task {
    workgraph::graph::Task {
        id: id.to_string(),
        title: title.to_string(),
        ..Default::default()
    }
}

/// Test unconfigured cycle auto-break-in behavior
#[test]
fn test_unconfigured_cycle_auto_breakin() {
    // Create a 3-task cycle without any CycleConfig: A → B → C → A
    let mut graph = WorkGraph::new();

    let mut task_a = make_task("a", "Task A");
    task_a.after = vec!["c".to_string()]; // A depends on C (cycle edge)
    task_a.status = Status::Open;

    let mut task_b = make_task("b", "Task B");
    task_b.after = vec!["a".to_string()]; // B depends on A
    task_b.status = Status::Open;

    let mut task_c = make_task("c", "Task C");
    task_c.after = vec!["b".to_string()]; // C depends on B
    task_c.status = Status::Open;

    // None of the tasks have cycle_config - this should auto-break
    assert!(task_a.cycle_config.is_none());
    assert!(task_b.cycle_config.is_none());
    assert!(task_c.cycle_config.is_none());

    graph.add_node(Node::Task(task_a));
    graph.add_node(Node::Task(task_b));
    graph.add_node(Node::Task(task_c));

    let cycle_analysis = CycleAnalysis::from_graph(&graph);

    // Verify that a cycle was detected
    assert!(!cycle_analysis.cycles.is_empty(), "Should detect the cycle");
    assert_eq!(
        cycle_analysis.cycles.len(),
        1,
        "Should be exactly one cycle"
    );

    // Get ready tasks - with auto-break-in, at least one should be ready despite the cycle
    let ready_tasks = ready_tasks_cycle_aware(&graph, &cycle_analysis);

    assert!(
        !ready_tasks.is_empty(),
        "Auto-break-in should make at least one task ready"
    );
    assert_eq!(
        ready_tasks.len(),
        1,
        "Exactly one task should be selected for auto-break-in"
    );

    // The break-in task should be deterministic (e.g., alphabetically first)
    let break_in_task = &ready_tasks[0];
    assert_eq!(
        break_in_task.id, "a",
        "Task 'a' should be selected for break-in (alphabetically first)"
    );
}

/// Test that configured cycles are NOT affected by auto-break-in logic
#[test]
fn test_configured_cycle_unaffected() {
    // Create the same cycle but with CycleConfig on one member
    let mut graph = WorkGraph::new();

    let mut task_a = make_task("a", "Task A");
    task_a.after = vec!["c".to_string()];
    task_a.status = Status::Open;
    // Add cycle config to make this a configured cycle
    task_a.cycle_config = Some(workgraph::graph::CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut task_b = make_task("b", "Task B");
    task_b.after = vec!["a".to_string()];
    task_b.status = Status::Open;

    let mut task_c = make_task("c", "Task C");
    task_c.after = vec!["b".to_string()];
    task_c.status = Status::Open;

    graph.add_node(Node::Task(task_a));
    graph.add_node(Node::Task(task_b));
    graph.add_node(Node::Task(task_c));

    let cycle_analysis = CycleAnalysis::from_graph(&graph);
    let ready_tasks = ready_tasks_cycle_aware(&graph, &cycle_analysis);

    // With proper CycleConfig, the existing logic should handle it normally
    // The cycle header (task with cycle_config) should be ready via back-edge exemption
    assert!(!ready_tasks.is_empty(), "Cycle header should be ready");

    // Find the ready task - should be the cycle header
    let ready_task = ready_tasks.iter().find(|t| t.cycle_config.is_some());
    assert!(
        ready_task.is_some(),
        "The task with cycle_config should be ready"
    );
}

/// Test mixed scenario: some cycles configured, some not
#[test]
fn test_mixed_configured_unconfigured_cycles() {
    let mut graph = WorkGraph::new();

    // First cycle: A → B → A (configured)
    let mut task_a = make_task("a", "Task A");
    task_a.after = vec!["b".to_string()];
    task_a.status = Status::Open;
    task_a.cycle_config = Some(workgraph::graph::CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let mut task_b = make_task("b", "Task B");
    task_b.after = vec!["a".to_string()];
    task_b.status = Status::Open;

    // Second cycle: X → Y → Z → X (unconfigured)
    let mut task_x = make_task("x", "Task X");
    task_x.after = vec!["z".to_string()];
    task_x.status = Status::Open;

    let mut task_y = make_task("y", "Task Y");
    task_y.after = vec!["x".to_string()];
    task_y.status = Status::Open;

    let mut task_z = make_task("z", "Task Z");
    task_z.after = vec!["y".to_string()];
    task_z.status = Status::Open;

    graph.add_node(Node::Task(task_a));
    graph.add_node(Node::Task(task_b));
    graph.add_node(Node::Task(task_x));
    graph.add_node(Node::Task(task_y));
    graph.add_node(Node::Task(task_z));

    let cycle_analysis = CycleAnalysis::from_graph(&graph);
    let ready_tasks = ready_tasks_cycle_aware(&graph, &cycle_analysis);

    // Should have ready tasks from both cycles
    assert_eq!(
        ready_tasks.len(),
        2,
        "Should have one ready task from each cycle"
    );

    // One should be the configured cycle header (task A)
    let configured_ready = ready_tasks.iter().find(|t| t.cycle_config.is_some());
    assert!(
        configured_ready.is_some(),
        "Configured cycle header should be ready"
    );

    // One should be the auto-break-in task from unconfigured cycle (task X, alphabetically first)
    let unconfigured_ready = ready_tasks.iter().find(|t| t.id == "x");
    assert!(
        unconfigured_ready.is_some(),
        "Auto-break-in task should be ready"
    );
}

// Tests for edit command cycle guards would go here,
// but they require access to the commands module which is not exposed
// in the library interface. The cycle detection logic is tested
// via the CLI interface during development.
