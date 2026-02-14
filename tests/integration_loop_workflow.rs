//! End-to-end integration tests for the full loop edge workflow via wg CLI.
//!
//! These tests exercise the wg binary directly (not library calls) to verify:
//! - `wg add --loops-to` and `--loop-max` create correct loop edges
//! - `wg show` displays loop_iteration
//! - `wg done` re-activates loop target and increments iteration
//! - `wg loops` displays all active loops
//! - Multiple iterations through a loop until max_iterations reached
//! - Loop with ready_after delay (task not immediately ready after re-activation)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get the path to the compiled `wg` binary
fn wg_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("could not get current exe path");
    path.pop(); // remove the binary name
    if path.ends_with("deps") {
        path.pop(); // remove deps/
    }
    path.push("wg");
    assert!(
        path.exists(),
        "wg binary not found at {:?}. Run `cargo build` first.",
        path
    );
    path
}

/// Run `wg` with given args in a specific workgraph directory
fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    let wg = wg_binary();
    Command::new(&wg)
        .arg("--dir")
        .arg(wg_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

/// Run `wg` and assert success, returning stdout as string
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

fn make_task(id: &str, title: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        description: None,
        status,
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
    }
}

fn setup_workgraph(tmp: &TempDir) -> PathBuf {
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let graph = WorkGraph::new();
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

fn graph_path(wg_dir: &Path) -> PathBuf {
    wg_dir.join("graph.jsonl")
}

// ===========================================================================
// Test 1: wg add with --loops-to and --loop-max creates correct loop edges
// ===========================================================================

#[test]
fn test_add_with_loops_to_creates_loop_edge() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // First add a target task
    wg_ok(&wg_dir, &["add", "Target Task", "--id", "target"]);

    // Add a task with a loop edge back to target
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Looping Task",
            "--id",
            "looper",
            "--blocked-by",
            "target",
            "--loops-to",
            "target",
            "--loop-max",
            "5",
        ],
    );

    // Load graph and verify loop edge was created
    let graph = load_graph(&graph_path(&wg_dir)).unwrap();
    let looper = graph.get_task("looper").unwrap();

    assert_eq!(
        looper.loops_to.len(),
        1,
        "Should have exactly one loop edge"
    );
    assert_eq!(looper.loops_to[0].target, "target");
    assert_eq!(looper.loops_to[0].max_iterations, 5);
    assert!(looper.loops_to[0].guard.is_none());
    assert!(looper.loops_to[0].delay.is_none());
}

#[test]
fn test_add_loops_to_requires_loop_max() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(&wg_dir, &["add", "Target", "--id", "target"]);

    // --loops-to without --loop-max should fail
    let output = wg_cmd(
        &wg_dir,
        &["add", "Bad Looper", "--id", "bad", "--loops-to", "target"],
    );
    assert!(!output.status.success(), "Should fail without --loop-max");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("loop-max"),
        "Error should mention --loop-max, got: {}",
        stderr
    );
}

#[test]
fn test_add_with_loop_delay() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(&wg_dir, &["add", "Target", "--id", "target"]);

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Delayed Looper",
            "--id",
            "delayed",
            "--loops-to",
            "target",
            "--loop-max",
            "3",
            "--loop-delay",
            "30s",
        ],
    );

    let graph = load_graph(&graph_path(&wg_dir)).unwrap();
    let task = graph.get_task("delayed").unwrap();
    assert_eq!(task.loops_to.len(), 1);
    assert_eq!(task.loops_to[0].delay.as_deref(), Some("30s"));
}

// ===========================================================================
// Test 2: wg show displays loop_iteration
// ===========================================================================

#[test]
fn test_show_displays_loop_iteration() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Create a task with loop_iteration > 0 by manually setting it in the graph
    let mut graph = WorkGraph::new();
    let mut task = make_task("iter-task", "Iterated Task", Status::Open);
    task.loop_iteration = 3;
    task.loops_to = vec![workgraph::graph::LoopEdge {
        target: "iter-task".to_string(),
        guard: None,
        max_iterations: 10,
        delay: None,
    }];
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path(&wg_dir)).unwrap();

    // wg show --json should include loop_iteration
    let output = wg_ok(&wg_dir, &["--json", "show", "iter-task"]);
    let json: serde_json::Value = serde_json::from_str(&output).expect("Should be valid JSON");

    assert_eq!(
        json["loop_iteration"], 3,
        "JSON should show loop_iteration=3"
    );
    assert!(
        json["loops_to"].is_array(),
        "JSON should include loops_to array"
    );
    assert_eq!(json["loops_to"][0]["target"], "iter-task");
    assert_eq!(json["loops_to"][0]["max_iterations"], 10);

    // Human-readable show should also mention iteration
    let human_output = wg_ok(&wg_dir, &["show", "iter-task"]);
    assert!(
        human_output.contains("iteration") || human_output.contains("Iteration"),
        "Human output should display iteration info, got:\n{}",
        human_output
    );
}

// ===========================================================================
// Test 3: wg done re-activates loop target and increments iteration
// ===========================================================================

#[test]
fn test_done_reactivates_loop_target() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Create a chain: target -> looper, where looper loops_to target
    let mut graph = WorkGraph::new();
    let target = make_task("target", "Target Task", Status::Done);
    let mut looper = make_task("looper", "Looper Task", Status::Done);
    looper.blocked_by = vec!["target".to_string()];
    looper.loops_to = vec![workgraph::graph::LoopEdge {
        target: "target".to_string(),
        guard: None,
        max_iterations: 5,
        delay: None,
    }];

    graph.add_node(Node::Task(target));
    graph.add_node(Node::Task(looper));
    save_graph(&graph, &graph_path(&wg_dir)).unwrap();

    // Mark looper as done via CLI (it's already Done, so we need it InProgress first)
    // Actually, let's set up a clean scenario:
    let mut graph = WorkGraph::new();
    let target = make_task("target", "Target Task", Status::Done);
    let mut looper = make_task("looper", "Looper Task", Status::InProgress);
    looper.blocked_by = vec!["target".to_string()];
    looper.loops_to = vec![workgraph::graph::LoopEdge {
        target: "target".to_string(),
        guard: None,
        max_iterations: 5,
        delay: None,
    }];
    looper.assigned = Some("test-agent".to_string());

    graph.add_node(Node::Task(target));
    graph.add_node(Node::Task(looper));
    save_graph(&graph, &graph_path(&wg_dir)).unwrap();

    // Mark looper as done via CLI
    let output = wg_ok(&wg_dir, &["done", "looper"]);

    // Stdout should mention loop re-activation
    assert!(
        output.contains("re-activated") || output.contains("Loop"),
        "Output should mention loop re-activation, got:\n{}",
        output
    );

    // Verify graph state: target should be re-opened with iteration incremented
    let graph = load_graph(&graph_path(&wg_dir)).unwrap();
    let target = graph.get_task("target").unwrap();
    assert_eq!(
        target.status,
        Status::Open,
        "Target should be re-opened by loop"
    );
    assert_eq!(
        target.loop_iteration, 1,
        "Target loop_iteration should be incremented to 1"
    );

    let looper = graph.get_task("looper").unwrap();
    assert_eq!(
        looper.status,
        Status::Open,
        "Looper (source) should be re-opened by loop"
    );
    assert_eq!(
        looper.loop_iteration, 1,
        "Looper loop_iteration should be incremented to 1"
    );
}

// ===========================================================================
// Test 4: wg loops displays all active loops
// ===========================================================================

#[test]
fn test_loops_displays_active_loops() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Create a graph with two loop edges
    let mut graph = WorkGraph::new();

    let mut task_a = make_task("a", "Task A", Status::Open);
    task_a.loops_to = vec![workgraph::graph::LoopEdge {
        target: "a".to_string(),
        guard: None,
        max_iterations: 10,
        delay: None,
    }];

    let task_b = make_task("b", "Task B", Status::Open);
    let mut task_c = make_task("c", "Task C", Status::Open);
    task_c.blocked_by = vec!["b".to_string()];
    task_c.loops_to = vec![workgraph::graph::LoopEdge {
        target: "b".to_string(),
        guard: None,
        max_iterations: 3,
        delay: Some("5m".to_string()),
    }];

    graph.add_node(Node::Task(task_a));
    graph.add_node(Node::Task(task_b));
    graph.add_node(Node::Task(task_c));
    save_graph(&graph, &graph_path(&wg_dir)).unwrap();

    // Human-readable output
    let output = wg_ok(&wg_dir, &["loops"]);
    assert!(
        output.contains("loops_to") || output.contains("Loop edges"),
        "Should display loop edge info, got:\n{}",
        output
    );
    // Both loops should appear
    assert!(
        output.contains("a") && output.contains("b"),
        "Should show both loop targets, got:\n{}",
        output
    );
    assert!(
        output.contains("ACTIVE"),
        "Active loops should be marked ACTIVE, got:\n{}",
        output
    );

    // JSON output
    let json_output = wg_ok(&wg_dir, &["--json", "loops"]);
    let json: serde_json::Value = serde_json::from_str(&json_output).expect("Should be valid JSON");
    let edges = json["loop_edges"]
        .as_array()
        .expect("Should have loop_edges array");
    assert_eq!(edges.len(), 2, "Should have 2 loop edges");

    // Verify one edge has delay
    let has_delay = edges.iter().any(|e| e.get("delay").is_some());
    assert!(has_delay, "At least one edge should have a delay");
}

#[test]
fn test_loops_shows_exhausted_loops() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Create a loop edge where current iteration == max
    let mut graph = WorkGraph::new();
    let mut task = make_task("exhausted", "Exhausted Task", Status::Done);
    task.loop_iteration = 5;
    task.loops_to = vec![workgraph::graph::LoopEdge {
        target: "exhausted".to_string(),
        guard: None,
        max_iterations: 5,
        delay: None,
    }];
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path(&wg_dir)).unwrap();

    let output = wg_ok(&wg_dir, &["loops"]);
    assert!(
        output.contains("EXHAUSTED"),
        "Should show EXHAUSTED for finished loop, got:\n{}",
        output
    );

    let json_output = wg_ok(&wg_dir, &["--json", "loops"]);
    let json: serde_json::Value = serde_json::from_str(&json_output).unwrap();
    let edges = json["loop_edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["status"], "exhausted");
}

// ===========================================================================
// Test 5: Multiple iterations through a loop until max_iterations reached
// ===========================================================================

#[test]
fn test_multiple_iterations_until_max() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Create a self-loop task with max_iterations = 3
    let mut graph = WorkGraph::new();
    let mut task = make_task("repeater", "Repeating Task", Status::Open);
    task.loops_to = vec![workgraph::graph::LoopEdge {
        target: "repeater".to_string(),
        guard: None,
        max_iterations: 3,
        delay: None,
    }];
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path(&wg_dir)).unwrap();

    // Iteration 1: complete -> re-opens
    wg_ok(&wg_dir, &["done", "repeater"]);
    let graph = load_graph(&graph_path(&wg_dir)).unwrap();
    let task = graph.get_task("repeater").unwrap();
    assert_eq!(task.status, Status::Open, "Iter 1: should re-open");
    assert_eq!(task.loop_iteration, 1, "Iter 1: loop_iteration should be 1");

    // Iteration 2: complete -> re-opens
    wg_ok(&wg_dir, &["done", "repeater"]);
    let graph = load_graph(&graph_path(&wg_dir)).unwrap();
    let task = graph.get_task("repeater").unwrap();
    assert_eq!(task.status, Status::Open, "Iter 2: should re-open");
    assert_eq!(task.loop_iteration, 2, "Iter 2: loop_iteration should be 2");

    // Iteration 3: complete -> re-opens (iteration goes 2->3, but 3 == max, so next time won't fire)
    wg_ok(&wg_dir, &["done", "repeater"]);
    let graph = load_graph(&graph_path(&wg_dir)).unwrap();
    let task = graph.get_task("repeater").unwrap();
    // After this done, evaluate_loop_edges checks if iteration < max_iterations
    // iteration was 2, 2 < 3 → fires, sets to 3
    assert_eq!(task.loop_iteration, 3, "Iter 3: loop_iteration should be 3");
    assert_eq!(
        task.status,
        Status::Open,
        "Iter 3: should still re-open (2 < 3)"
    );

    // Iteration 4: complete -> stays Done (3 >= 3, loop exhausted)
    wg_ok(&wg_dir, &["done", "repeater"]);
    let graph = load_graph(&graph_path(&wg_dir)).unwrap();
    let task = graph.get_task("repeater").unwrap();
    assert_eq!(
        task.status,
        Status::Done,
        "Iter 4: should stay Done since max_iterations reached"
    );
    assert_eq!(
        task.loop_iteration, 3,
        "Iter 4: loop_iteration should remain 3"
    );

    // Verify via wg loops --json that the loop is exhausted
    let json_output = wg_ok(&wg_dir, &["--json", "loops"]);
    let json: serde_json::Value = serde_json::from_str(&json_output).unwrap();
    let edges = json["loop_edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["status"], "exhausted");
    assert_eq!(edges[0]["current_iteration"], 3);
}

// ===========================================================================
// Test 6: Loop with ready_after delay
// ===========================================================================

#[test]
fn test_loop_with_ready_after_delay() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Create a self-loop task with a 1-hour delay
    let mut graph = WorkGraph::new();
    let mut task = make_task("delayed", "Delayed Loop Task", Status::Open);
    task.loops_to = vec![workgraph::graph::LoopEdge {
        target: "delayed".to_string(),
        guard: None,
        max_iterations: 5,
        delay: Some("1h".to_string()),
    }];
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path(&wg_dir)).unwrap();

    // Complete the task to trigger the loop
    wg_ok(&wg_dir, &["done", "delayed"]);

    // Verify the task is re-opened but has ready_after set
    let graph = load_graph(&graph_path(&wg_dir)).unwrap();
    let task = graph.get_task("delayed").unwrap();
    assert_eq!(task.status, Status::Open, "Task should be re-opened");
    assert_eq!(task.loop_iteration, 1);
    assert!(
        task.ready_after.is_some(),
        "ready_after should be set for delayed loop"
    );

    // Parse ready_after and verify it's in the future (~1 hour from now)
    let ready_after: chrono::DateTime<chrono::Utc> =
        task.ready_after.as_ref().unwrap().parse().unwrap();
    let now = chrono::Utc::now();
    let diff_secs = (ready_after - now).num_seconds();
    assert!(
        diff_secs > 3500 && diff_secs < 3700,
        "ready_after should be ~3600s from now, got {}s",
        diff_secs
    );

    // Verify the task is NOT marked as ready (since ready_after is in the future)
    let ready_output = wg_ok(&wg_dir, &["--json", "ready"]);
    let ready_json: serde_json::Value = serde_json::from_str(&ready_output).unwrap();
    let ready_tasks = ready_json.as_array().unwrap();
    // The task may appear in the list but should have ready: false
    let delayed_entry = ready_tasks.iter().find(|t| t["id"] == "delayed");
    if let Some(entry) = delayed_entry {
        assert_eq!(
            entry["ready"], false,
            "Delayed task should have ready=false, got:\n{}",
            ready_output
        );
    }
    // Either way, it should not be truly ready

    // wg show --json should display the ready_after field
    let show_output = wg_ok(&wg_dir, &["--json", "show", "delayed"]);
    let json: serde_json::Value = serde_json::from_str(&show_output).unwrap();
    assert!(
        json["ready_after"].is_string(),
        "show JSON should include ready_after"
    );
}

// ===========================================================================
// Test 7: Chain loop — A → B → C where C loops_to A via CLI
// ===========================================================================

#[test]
fn test_chain_loop_via_cli() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Build chain: A → B → C, where C loops_to A
    wg_ok(&wg_dir, &["add", "Task A", "--id", "a"]);
    wg_ok(
        &wg_dir,
        &["add", "Task B", "--id", "b", "--blocked-by", "a"],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task C",
            "--id",
            "c",
            "--blocked-by",
            "b",
            "--loops-to",
            "a",
            "--loop-max",
            "3",
        ],
    );

    // Complete A, B, C in order
    wg_ok(&wg_dir, &["done", "a"]);
    wg_ok(&wg_dir, &["done", "b"]);
    let done_output = wg_ok(&wg_dir, &["done", "c"]);

    // C's done should trigger loop re-activation of A
    assert!(
        done_output.contains("re-activated"),
        "Should mention re-activation, got:\n{}",
        done_output
    );

    // Verify: A should be re-opened with loop_iteration=1
    let graph = load_graph(&graph_path(&wg_dir)).unwrap();
    let a = graph.get_task("a").unwrap();
    assert_eq!(a.status, Status::Open, "A should be re-opened");
    assert_eq!(a.loop_iteration, 1);

    // B should also be re-opened (intermediate task between A and C)
    let b = graph.get_task("b").unwrap();
    assert_eq!(
        b.status,
        Status::Open,
        "B should be re-opened as intermediate"
    );

    // C should be re-opened (it's the loop source but is part of the cycle)
    let c = graph.get_task("c").unwrap();
    assert_eq!(
        c.status,
        Status::Open,
        "C (source) should be re-opened by loop"
    );
    assert_eq!(c.loop_iteration, 1);
}

// ===========================================================================
// Test 8: wg add --loops-to creates edge visible in wg show
// ===========================================================================

#[test]
fn test_add_loop_edge_visible_in_show() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    wg_ok(&wg_dir, &["add", "Start", "--id", "start"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "End",
            "--id",
            "end",
            "--loops-to",
            "start",
            "--loop-max",
            "7",
        ],
    );

    // wg show end --json should show the loop edge
    let output = wg_ok(&wg_dir, &["--json", "show", "end"]);
    let json: serde_json::Value = serde_json::from_str(&output).unwrap();
    let loops_to = json["loops_to"].as_array().unwrap();
    assert_eq!(loops_to.len(), 1);
    assert_eq!(loops_to[0]["target"], "start");
    assert_eq!(loops_to[0]["max_iterations"], 7);
}

// ===========================================================================
// Test 9: Complete loop lifecycle via CLI only (no library graph manipulation)
// ===========================================================================

#[test]
fn test_full_loop_lifecycle_cli_only() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp);

    // Create a self-looping task entirely via CLI
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Self Looper",
            "--id",
            "self-loop",
            "--loops-to",
            "self-loop",
            "--loop-max",
            "2",
        ],
    );

    // Verify initial state
    let json_output = wg_ok(&wg_dir, &["--json", "show", "self-loop"]);
    let json: serde_json::Value = serde_json::from_str(&json_output).unwrap();
    assert_eq!(json["status"], "open");
    // loop_iteration is 0 and gets skipped in serialization when zero
    let iter = json
        .get("loop_iteration")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    assert_eq!(iter, 0);

    // First done: should loop (0 < 2)
    wg_ok(&wg_dir, &["done", "self-loop"]);

    let json_output = wg_ok(&wg_dir, &["--json", "show", "self-loop"]);
    let json: serde_json::Value = serde_json::from_str(&json_output).unwrap();
    assert_eq!(json["status"], "open", "Should re-open after first done");
    assert_eq!(json["loop_iteration"], 1);

    // Second done: should loop (1 < 2)
    wg_ok(&wg_dir, &["done", "self-loop"]);

    let json_output = wg_ok(&wg_dir, &["--json", "show", "self-loop"]);
    let json: serde_json::Value = serde_json::from_str(&json_output).unwrap();
    assert_eq!(json["status"], "open", "Should re-open after second done");
    assert_eq!(json["loop_iteration"], 2);

    // Third done: should NOT loop (2 >= 2, exhausted)
    wg_ok(&wg_dir, &["done", "self-loop"]);

    let json_output = wg_ok(&wg_dir, &["--json", "show", "self-loop"]);
    let json: serde_json::Value = serde_json::from_str(&json_output).unwrap();
    assert_eq!(json["status"], "done", "Should stay done when max reached");
    assert_eq!(json["loop_iteration"], 2, "Iteration should remain 2");

    // wg loops should show it as exhausted
    let loops_output = wg_ok(&wg_dir, &["--json", "loops"]);
    let loops_json: serde_json::Value = serde_json::from_str(&loops_output).unwrap();
    let edges = loops_json["loop_edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["status"], "exhausted");
}
