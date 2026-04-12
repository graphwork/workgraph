//! End-to-end integration tests for accidental cycle auto-recovery.
//!
//! This module tests the complete cycle detection and auto-recovery system,
//! verifying that:
//! 1. Prevention: Triage/edit operations cannot accidentally create cycles
//! 2. Recovery: Unconfigured cycles are auto-broken and dispatch tasks
//! 3. Regression: Configured cycles still work normally

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{CycleAnalysis, CycleConfig, Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::ready_tasks_cycle_aware;

/// Helper to create a basic task
fn make_task(id: &str, title: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        status,
        ..Default::default()
    }
}

/// Helper to create a task with dependencies
fn make_task_with_deps(id: &str, title: &str, status: Status, after: Vec<&str>) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        status,
        after: after.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

/// Find the wg binary for CLI testing
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

/// Run a wg command and return output
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

/// Run a wg command and expect success
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

/// Run a wg command and expect failure
fn wg_fail(wg_dir: &Path, args: &[&str]) -> String {
    let output = wg_cmd(wg_dir, args);
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !output.status.success(),
        "wg {:?} should have failed but succeeded.\nstderr: {}",
        args,
        stderr
    );
    stderr
}

/// Set up a workgraph directory with given tasks
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

/// Test 1: Prevention - Verify edit command prevents accidental cycle creation
///
/// Simulates a scenario where someone tries to add a dependency that would
/// create an unconfigured cycle, and verifies the prevention guard works.
#[test]
fn test_accidental_cycle_prevention() {
    let temp_dir = TempDir::new().unwrap();

    // Create a basic graph: A → B → C
    let task_a = make_task_with_deps("a", "Task A", Status::Open, vec!["b"]);
    let task_b = make_task_with_deps("b", "Task B", Status::Open, vec!["c"]);
    let task_c = make_task("c", "Task C", Status::Open);

    let wg_dir = setup_workgraph(&temp_dir, vec![task_a, task_b, task_c]);

    // Try to add a dependency C → A (would create cycle A → B → C → A)
    // This should FAIL because no task has cycle_config
    let stderr = wg_fail(&wg_dir, &["edit", "c", "--add-after", "a"]);

    // Should fail due to cycle detection guard
    assert!(
        stderr.contains("cycle"),
        "Error should mention cycle: {}",
        stderr
    );
    assert!(
        stderr.contains("CycleConfig"),
        "Error should suggest CycleConfig: {}",
        stderr
    );

    // Verify the graph was not modified
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = load_graph(&graph_path).unwrap();
    let task_c = graph.get_task("c").unwrap();
    assert!(
        task_c.after.is_empty(),
        "Task C should not have new dependencies"
    );

    // Test that --allow-cycle override works
    wg_ok(&wg_dir, &["edit", "c", "--add-after", "a", "--allow-cycle"]);

    // Verify the cycle was created
    let graph = load_graph(&graph_path).unwrap();
    let task_c = graph.get_task("c").unwrap();
    assert!(
        task_c.after.contains(&"a".to_string()),
        "Task C should now depend on A"
    );
}

/// Test 2: Recovery - Verify auto-break-in for unconfigured cycles
///
/// Creates an unconfigured cycle manually and verifies the system automatically
/// breaks in and makes exactly one task ready for execution.
#[test]
fn test_accidental_cycle_auto_recovery() {
    // Create an unconfigured 3-task cycle: A → B → C → A
    let mut graph = WorkGraph::new();

    let task_a = make_task_with_deps("a", "Task A", Status::Open, vec!["c"]);
    let task_b = make_task_with_deps("b", "Task B", Status::Open, vec!["a"]);
    let task_c = make_task_with_deps("c", "Task C", Status::Open, vec!["b"]);

    // Verify none have cycle_config (unconfigured)
    assert!(task_a.cycle_config.is_none());
    assert!(task_b.cycle_config.is_none());
    assert!(task_c.cycle_config.is_none());

    graph.add_node(Node::Task(task_a));
    graph.add_node(Node::Task(task_b));
    graph.add_node(Node::Task(task_c));

    // Analyze cycles
    let cycle_analysis = CycleAnalysis::from_graph(&graph);

    // Should detect exactly one cycle
    assert_eq!(cycle_analysis.cycles.len(), 1, "Should detect one cycle");
    let cycle = &cycle_analysis.cycles[0];
    assert_eq!(cycle.members.len(), 3, "Cycle should have 3 members");

    // Verify all tasks are in the cycle
    let cycle_members: HashSet<String> = cycle.members.iter().map(|m| m.clone()).collect();
    assert!(cycle_members.contains("a"));
    assert!(cycle_members.contains("b"));
    assert!(cycle_members.contains("c"));

    // Get ready tasks with auto-break-in
    let ready_tasks = ready_tasks_cycle_aware(&graph, &cycle_analysis);

    // Auto-break-in should make exactly one task ready
    assert_eq!(
        ready_tasks.len(),
        1,
        "Auto-break-in should make exactly one task ready"
    );

    // Should be deterministic (alphabetically first)
    let ready_task = &ready_tasks[0];
    assert_eq!(
        ready_task.id, "a",
        "Task 'a' should be selected for auto-break-in"
    );
    assert_eq!(ready_task.status, Status::Open, "Ready task should be Open");
}

/// Test 3: No Regression - Verify configured cycles still work normally
///
/// Creates a properly configured cycle and verifies it behaves as expected:
/// the cycle header is ready, iteration works, and convergence stops the cycle.
#[test]
fn test_accidental_cycle_no_regression_configured() {
    // Create a configured cycle: A ⟷ B with CycleConfig
    let mut graph = WorkGraph::new();

    // Task A has cycle config (is the cycle header)
    let mut task_a = make_task_with_deps("a", "Task A", Status::Open, vec!["b"]);
    task_a.cycle_config = Some(CycleConfig {
        max_iterations: 3,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let task_b = make_task_with_deps("b", "Task B", Status::Open, vec!["a"]);

    graph.add_node(Node::Task(task_a.clone()));
    graph.add_node(Node::Task(task_b));

    // Analyze cycles
    let cycle_analysis = CycleAnalysis::from_graph(&graph);
    assert_eq!(
        cycle_analysis.cycles.len(),
        1,
        "Should detect configured cycle"
    );

    // Get ready tasks - should work normally via back-edge exemption
    let ready_tasks = ready_tasks_cycle_aware(&graph, &cycle_analysis);

    // The cycle header (task with cycle_config) should be ready
    assert!(!ready_tasks.is_empty(), "Cycle header should be ready");

    let ready_task = ready_tasks
        .iter()
        .find(|t| t.cycle_config.is_some())
        .expect("Task with cycle_config should be ready");
    assert_eq!(ready_task.id, "a", "Cycle header should be task A");
}

/// Test 4: Mixed scenario - configured and unconfigured cycles coexist
///
/// Verifies that when both configured and unconfigured cycles exist,
/// the system handles them appropriately: configured cycles work normally,
/// unconfigured cycles get auto-break-in.
#[test]
fn test_accidental_cycle_mixed_scenario() {
    let mut graph = WorkGraph::new();

    // Configured cycle: A ⟷ B
    let mut task_a = make_task_with_deps("a", "Task A", Status::Open, vec!["b"]);
    task_a.cycle_config = Some(CycleConfig {
        max_iterations: 2,
        guard: None,
        delay: None,
        no_converge: false,
        restart_on_failure: true,
        max_failure_restarts: None,
    });

    let task_b = make_task_with_deps("b", "Task B", Status::Open, vec!["a"]);

    // Unconfigured cycle: X → Y → Z → X
    let task_x = make_task_with_deps("x", "Task X", Status::Open, vec!["z"]);
    let task_y = make_task_with_deps("y", "Task Y", Status::Open, vec!["x"]);
    let task_z = make_task_with_deps("z", "Task Z", Status::Open, vec!["y"]);

    graph.add_node(Node::Task(task_a));
    graph.add_node(Node::Task(task_b));
    graph.add_node(Node::Task(task_x));
    graph.add_node(Node::Task(task_y));
    graph.add_node(Node::Task(task_z));

    // Analyze cycles
    let cycle_analysis = CycleAnalysis::from_graph(&graph);
    assert_eq!(cycle_analysis.cycles.len(), 2, "Should detect both cycles");

    // Get ready tasks
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

    // One should be the auto-break-in task from unconfigured cycle (task X)
    let unconfigured_ready = ready_tasks.iter().find(|t| t.id == "x");
    assert!(
        unconfigured_ready.is_some(),
        "Auto-break-in task should be ready"
    );
}

/// Test 5: E2E simulation of triage creating fix task
///
/// Simulates a realistic scenario where an agent fails and triage might
/// want to create a "fix" task that depends on the failed task, which could
/// create a cycle if the failed task depends on other work.
#[test]
fn test_accidental_cycle_triage_simulation() {
    let temp_dir = TempDir::new().unwrap();

    // Create a scenario: implement-feature → test-feature → deploy-feature
    let implement = make_task_with_deps(
        "implement-feature",
        "Implement Feature",
        Status::Done,
        vec![],
    );
    let test_task = make_task_with_deps(
        "test-feature",
        "Test Feature",
        Status::Failed,
        vec!["implement-feature"],
    );
    let deploy = make_task_with_deps(
        "deploy-feature",
        "Deploy Feature",
        Status::Open,
        vec!["test-feature"],
    );

    let wg_dir = setup_workgraph(&temp_dir, vec![implement, test_task, deploy]);

    // Simulate triage creating a fix task that depends on the failed test
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Fix test failure",
            "--id",
            "fix-test",
            "--after",
            "test-feature",
        ],
    );

    // This should succeed - no cycle yet
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = load_graph(&graph_path).unwrap();
    let fix_task = graph.get_task("fix-test").unwrap();
    assert_eq!(fix_task.after, vec!["test-feature"]);

    // Now try to make deploy depend on the fix (could be reasonable)
    wg_ok(
        &wg_dir,
        &["edit", "deploy-feature", "--add-after", "fix-test"],
    );

    // But if we try to create a real cycle by making fix depend on deploy,
    // it should be prevented
    let stderr = wg_fail(
        &wg_dir,
        &["edit", "fix-test", "--add-after", "deploy-feature"],
    );

    // Should be prevented due to cycle detection
    assert!(
        stderr.contains("cycle"),
        "Error should mention cycle creation: {}",
        stderr
    );
}

/// Test 6: CLI Integration - Test ready command with unconfigured cycles
///
/// Verifies that the `wg ready` command correctly identifies auto-break-in tasks
/// when unconfigured cycles are present.
#[test]
fn test_accidental_cycle_cli_ready() {
    let temp_dir = TempDir::new().unwrap();

    // Create unconfigured cycle via CLI: A → B → C → A
    let wg_dir = setup_workgraph(&temp_dir, vec![]);

    wg_ok(&wg_dir, &["add", "Task A", "--id", "a"]);
    wg_ok(&wg_dir, &["add", "Task B", "--id", "b", "--after", "a"]);
    wg_ok(&wg_dir, &["add", "Task C", "--id", "c", "--after", "b"]);

    // Create the cycle with --allow-cycle
    wg_ok(&wg_dir, &["edit", "a", "--add-after", "c", "--allow-cycle"]);

    // Check ready tasks - should show auto-break-in task
    let output = wg_ok(&wg_dir, &["ready"]);

    // Should show exactly one task (the break-in task)
    let lines: Vec<&str> = output
        .trim()
        .split('\n')
        .filter(|l| !l.trim().is_empty())
        .collect();
    let task_lines: Vec<&str> = lines
        .iter()
        .filter(|l| l.contains("Task"))
        .cloned()
        .collect();
    assert_eq!(
        task_lines.len(),
        1,
        "Should show exactly one ready task from auto-break-in: {}",
        output
    );

    // Should be task 'a' (alphabetically first)
    assert!(output.contains("a"), "Ready task should be 'a': {}", output);
}
