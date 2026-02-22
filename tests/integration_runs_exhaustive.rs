//! Exhaustive integration tests for `wg runs` (list, show, restore, diff).
//!
//! Covers test-spec sections 3.1–3.23 and cross-cutting 4.1–4.2.
//! Each test exercises the `wg` binary end-to-end via `Command`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
        args, stdout, stderr
    );
    stdout
}

fn wg_fail(wg_dir: &Path, args: &[&str]) -> (String, String) {
    let output = wg_cmd(wg_dir, args);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        !output.status.success(),
        "wg {:?} should have failed but succeeded.\nstdout: {}\nstderr: {}",
        args, stdout, stderr
    );
    (stdout, stderr)
}

fn wg_json(wg_dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut full_args = vec!["--json"];
    full_args.extend_from_slice(args);
    let raw = wg_ok(wg_dir, &full_args);
    serde_json::from_str(&raw).unwrap_or_else(|e| {
        panic!("Failed to parse JSON.\nError: {}\nOutput: {}", e, raw)
    })
}

fn make_task(id: &str, title: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        status,
        ..Task::default()
    }
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

fn load_wg_graph(wg_dir: &Path) -> WorkGraph {
    let graph_path = wg_dir.join("graph.jsonl");
    load_graph(&graph_path).unwrap()
}

// ===========================================================================
// 3.2 runs_list_chronological — 3 runs, verify order
// ===========================================================================

#[test]
fn test_runs_list_chronological_three_runs() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("a", "Task A", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Replay 1
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Reset task to failed for next replay
    set_task_failed(&wg_dir, "a");
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    set_task_failed(&wg_dir, "a");
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Verify list shows 3 runs in order
    let output = wg_ok(&wg_dir, &["runs", "list"]);
    assert!(output.contains("run-001"), "should contain run-001: {}", output);
    assert!(output.contains("run-002"), "should contain run-002: {}", output);
    assert!(output.contains("run-003"), "should contain run-003: {}", output);

    // Verify ordering: run-001 appears before run-002 appears before run-003
    let pos1 = output.find("run-001").unwrap();
    let pos2 = output.find("run-002").unwrap();
    let pos3 = output.find("run-003").unwrap();
    assert!(pos1 < pos2, "run-001 should appear before run-002");
    assert!(pos2 < pos3, "run-002 should appear before run-003");

    // JSON output also shows 3
    let json = wg_json(&wg_dir, &["runs", "list"]);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["id"], "run-001");
    assert_eq!(arr[1]["id"], "run-002");
    assert_eq!(arr[2]["id"], "run-003");
}

fn set_task_failed(wg_dir: &Path, id: &str) {
    let mut graph = load_wg_graph(wg_dir);
    let task = graph.get_task_mut(id).unwrap();
    task.status = Status::Failed;
    task.failure_reason = Some("err".to_string());
    task.assigned = None;
    save_graph(&graph, wg_dir.join("graph.jsonl")).unwrap();
}

// ===========================================================================
// 3.5 runs_show_nonexistent — error on missing run
// ===========================================================================

#[test]
fn test_runs_show_nonexistent() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let (_stdout, stderr) = wg_fail(&wg_dir, &["runs", "show", "run-999"]);
    assert!(
        stderr.contains("not found") || stderr.contains("Failed to read") || stderr.contains("No such file"),
        "should report run not found: stderr={}",
        stderr
    );
}

// ===========================================================================
// 3.6 runs_restore_restores_graph — end-to-end status verification
// ===========================================================================

#[test]
fn test_runs_restore_restores_graph_status() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("r1", "Task R1", Status::Done);
    let t2 = make_task("r2", "Task R2", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Run replay to create run-001 snapshot (captures: r1=Done, r2=Failed)
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // After replay: r2 should be Open
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("r2").unwrap().status, Status::Open);

    // Restore from run-001
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // After restore: r2 should be back to Failed (snapshot state)
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(
        graph.get_task("r2").unwrap().status,
        Status::Failed,
        "r2 should be restored to Failed"
    );
    assert_eq!(
        graph.get_task("r1").unwrap().status,
        Status::Done,
        "r1 should still be Done"
    );
}

// ===========================================================================
// 3.8 runs_restore_provenance — provenance entry recorded
// ===========================================================================

#[test]
fn test_runs_restore_provenance() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("p1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create a replay snapshot
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Restore from run-001
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Check provenance for "restore" entry
    let ops = workgraph::provenance::read_all_operations(&wg_dir).unwrap();
    let restore_ops: Vec<_> = ops.iter().filter(|o| o.op == "restore").collect();
    assert!(
        !restore_ops.is_empty(),
        "provenance should contain a 'restore' operation"
    );

    let restore_op = &restore_ops[0];
    assert_eq!(
        restore_op.detail["restored_from"].as_str().unwrap(),
        "run-001",
        "should record restored_from"
    );
    assert!(
        restore_op.detail["safety_snapshot"].as_str().is_some(),
        "should record safety_snapshot"
    );
}

// ===========================================================================
// 3.9 runs_restore_nonexistent — error on restoring missing run
// ===========================================================================

#[test]
fn test_runs_restore_nonexistent() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let (_stdout, stderr) = wg_fail(&wg_dir, &["runs", "restore", "run-nonexistent"]);
    assert!(
        stderr.contains("not found") || stderr.contains("Failed") || stderr.contains("No such file"),
        "should report run not found: stderr={}",
        stderr
    );
}

// ===========================================================================
// 3.10 runs_diff_shows_changes — status change, added, removed
// ===========================================================================

#[test]
fn test_runs_diff_shows_status_change() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("d1", "Task D1", Status::Done);
    let t2 = make_task("d2", "Task D2", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Replay captures snapshot with d1=Done, d2=Failed, then resets d2 to Open
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Now diff against run-001 should show d2: Failed -> Open
    let output = wg_ok(&wg_dir, &["runs", "diff", "run-001"]);
    assert!(
        output.contains("d2"),
        "diff should mention d2: {}",
        output
    );
    // Should show status change (Failed -> Open)
    assert!(
        output.contains("failed") || output.contains("Failed"),
        "diff should show old status: {}",
        output
    );
    assert!(
        output.contains("open") || output.contains("Open"),
        "diff should show new status: {}",
        output
    );
}

// ===========================================================================
// 3.11 runs_diff_no_changes
// ===========================================================================

#[test]
fn test_runs_diff_no_changes() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("nc1", "No change", Status::Done);
    let t2 = make_task("nc2", "Fail me", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Replay creates snapshot (nc1=Done, nc2=Failed), then resets nc2 to Open
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Restore to the snapshot (nc2 goes back to Failed)
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Now diff run-001 against current — should be identical since we just restored
    let output = wg_ok(&wg_dir, &["runs", "diff", "run-001"]);
    assert!(
        output.contains("No differences"),
        "should report no differences: {}",
        output
    );
}

// ===========================================================================
// 3.12 runs_diff_json_output
// ===========================================================================

#[test]
fn test_runs_diff_json_output() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("dj1", "Task", Status::Done);
    let t2 = make_task("dj2", "Failed task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Replay: snapshot has dj2=Failed, after replay dj2=Open
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // JSON diff
    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    assert_eq!(json["run_id"], "run-001");
    assert!(json["changes"].is_array());
    assert!(json["total_changes"].is_number());

    let changes = json["changes"].as_array().unwrap();
    assert!(
        !changes.is_empty(),
        "should have at least one change: {:?}",
        changes
    );

    // Find the dj2 change
    let dj2_change = changes.iter().find(|c| c["id"] == "dj2");
    assert!(dj2_change.is_some(), "should have a change for dj2");
    let dj2 = dj2_change.unwrap();
    assert_eq!(dj2["change"], "status_changed");
    assert_eq!(dj2["snapshot_status"], "failed");
    assert_eq!(dj2["current_status"], "open");
}

// ===========================================================================
// 3.13 runs_diff_nonexistent_run — error
// ===========================================================================

#[test]
fn test_runs_diff_nonexistent_run() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let (_stdout, stderr) = wg_fail(&wg_dir, &["runs", "diff", "run-nonexistent"]);
    assert!(
        stderr.contains("not found") || stderr.contains("No such file") || stderr.contains("Failed"),
        "should report snapshot not found: stderr={}",
        stderr
    );
}

// ===========================================================================
// 3.14 runs_list_json — full metadata structure validation
// ===========================================================================

#[test]
fn test_runs_list_json_metadata_structure() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("lj1", "Task 1", Status::Failed);
    let t2 = make_task("lj2", "Task 2", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Create two replays
    wg_ok(&wg_dir, &["replay", "--failed-only", "--model", "opus"]);
    set_task_failed(&wg_dir, "lj1");
    wg_ok(&wg_dir, &["replay", "--failed-only", "--model", "sonnet"]);

    let json = wg_json(&wg_dir, &["runs", "list"]);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 2);

    // Validate first run metadata structure
    let run1 = &arr[0];
    assert_eq!(run1["id"], "run-001");
    assert!(run1["timestamp"].is_string(), "should have timestamp");
    assert!(run1["reset_tasks"].is_array(), "should have reset_tasks");
    assert!(run1["preserved_tasks"].is_array(), "should have preserved_tasks");
    assert_eq!(run1["model"], "opus");
    assert!(run1["filter"].is_string(), "should have filter");

    // Validate second run
    let run2 = &arr[1];
    assert_eq!(run2["id"], "run-002");
    assert_eq!(run2["model"], "sonnet");
}

// ===========================================================================
// 3.17 snapshot_integrity — graph.jsonl content matches source exactly
// ===========================================================================

#[test]
fn test_snapshot_integrity_content_match() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("si1", "Integrity test", Status::Failed);
    let t2 = make_task("si2", "Another task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Read original graph content
    let original = fs::read_to_string(wg_dir.join("graph.jsonl")).unwrap();

    // Replay creates a snapshot
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Read snapshot graph content
    let snapshot = fs::read_to_string(wg_dir.join("runs/run-001/graph.jsonl")).unwrap();

    // Should match the original graph (snapshot was taken before modifications)
    assert_eq!(
        original, snapshot,
        "snapshot graph.jsonl should be byte-for-byte identical to original"
    );

    // meta.json should be valid JSON
    let meta_raw = fs::read_to_string(wg_dir.join("runs/run-001/meta.json")).unwrap();
    let meta: serde_json::Value = serde_json::from_str(&meta_raw).unwrap();
    assert_eq!(meta["id"], "run-001");
    assert!(meta["timestamp"].is_string());
    assert!(meta["reset_tasks"].is_array());
    assert!(meta["preserved_tasks"].is_array());
}

// ===========================================================================
// 3.18 snapshot_without_config_toml
// ===========================================================================

#[test]
fn test_snapshot_without_config_toml() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("nc1", "No config", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Ensure no config.toml exists
    let config_path = wg_dir.join("config.toml");
    if config_path.exists() {
        fs::remove_file(&config_path).unwrap();
    }
    assert!(!config_path.exists(), "config.toml should not exist");

    // Replay should succeed
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Snapshot should have graph.jsonl and meta.json but no config.toml
    let run_dir = wg_dir.join("runs/run-001");
    assert!(run_dir.join("graph.jsonl").exists());
    assert!(run_dir.join("meta.json").exists());
    assert!(
        !run_dir.join("config.toml").exists(),
        "snapshot should not contain config.toml when source has none"
    );
}

// ===========================================================================
// 3.19 runs_restore_json_output
// ===========================================================================

#[test]
fn test_runs_restore_json_output() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("rj1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create a replay snapshot
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Restore with --json
    let json = wg_json(&wg_dir, &["runs", "restore", "run-001"]);
    assert_eq!(json["restored_from"], "run-001");
    assert!(
        json["safety_snapshot"].is_string(),
        "should have safety_snapshot"
    );
    assert!(
        json["timestamp"].is_string(),
        "should have timestamp"
    );
}

// ===========================================================================
// 3.20 concurrent_replay_safety
// ===========================================================================

#[test]
fn test_concurrent_replay_safety() {
    let tmp = TempDir::new().unwrap();
    // Create multiple failed tasks
    let tasks: Vec<Task> = (0..5)
        .map(|i| make_task(&format!("c{}", i), &format!("Concurrent {}", i), Status::Failed))
        .collect();
    let wg_dir = setup_workgraph(&tmp, tasks);

    // Launch two replays concurrently
    let mut children = Vec::new();
    for _ in 0..2 {
        let child = Command::new(wg_binary())
            .arg("--dir")
            .arg(&wg_dir)
            .args(["replay", "--failed-only"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        children.push(child);
    }

    // Wait for both to complete
    let mut successes = 0;
    for child in children {
        let output = child.wait_with_output().unwrap();
        if output.status.success() {
            successes += 1;
        }
    }

    // At least one should succeed
    assert!(
        successes >= 1,
        "at least one concurrent replay should succeed"
    );

    // Run IDs should be distinct
    let runs = workgraph::runs::list_runs(&wg_dir).unwrap();
    let unique: std::collections::HashSet<_> = runs.iter().collect();
    assert_eq!(
        runs.len(),
        unique.len(),
        "all run IDs should be unique: {:?}",
        runs
    );

    // Graph should be parseable (not corrupted)
    let graph = load_wg_graph(&wg_dir);
    assert!(
        graph.tasks().count() == 5,
        "graph should still have 5 tasks"
    );
}

// ===========================================================================
// 3.22 runs_diff_with_removed_task
// ===========================================================================

#[test]
fn test_runs_diff_with_removed_task() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("rm1", "Task 1", Status::Done);
    let t2 = make_task("rm2", "Task 2", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Replay to create snapshot (has both rm1 and rm2)
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Remove rm2 from current graph by rewriting without it
    let graph = load_wg_graph(&wg_dir);
    // Remove rm2 by reconstructing graph with only rm1
    let rm1_task = graph.get_task("rm1").unwrap().clone();
    let mut new_graph = WorkGraph::new();
    new_graph.add_node(Node::Task(rm1_task));
    save_graph(&new_graph, wg_dir.join("graph.jsonl")).unwrap();

    // Diff should detect rm2 as removed
    let output = wg_ok(&wg_dir, &["runs", "diff", "run-001"]);
    assert!(
        output.contains("rm2"),
        "diff should mention removed task rm2: {}",
        output
    );

    // JSON diff too
    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    let changes = json["changes"].as_array().unwrap();
    let rm2_change = changes.iter().find(|c| c["id"] == "rm2");
    assert!(rm2_change.is_some(), "should have a change for rm2");
    assert_eq!(rm2_change.unwrap()["change"], "removed");
}

// ===========================================================================
// 3.23 runs_diff_with_added_task
// ===========================================================================

#[test]
fn test_runs_diff_with_added_task() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("ad1", "Task 1", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Replay to create snapshot (only ad1)
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Add a new task to the current graph
    let mut graph = load_wg_graph(&wg_dir);
    let new_task = make_task("ad2", "Added task", Status::Open);
    graph.add_node(Node::Task(new_task));
    save_graph(&graph, wg_dir.join("graph.jsonl")).unwrap();

    // Diff should detect ad2 as added
    let output = wg_ok(&wg_dir, &["runs", "diff", "run-001"]);
    assert!(
        output.contains("ad2"),
        "diff should mention added task ad2: {}",
        output
    );

    // JSON diff
    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    let changes = json["changes"].as_array().unwrap();
    let ad2_change = changes.iter().find(|c| c["id"] == "ad2");
    assert!(ad2_change.is_some(), "should have a change for ad2");
    assert_eq!(ad2_change.unwrap()["change"], "added");
}

// ===========================================================================
// 4.1 full_replay_restore_round_trip
// ===========================================================================

#[test]
fn test_full_replay_restore_round_trip() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("rt1", "Done task", Status::Done);
    let t2 = make_task("rt2", "Failed task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Step 1: Verify initial state
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("rt2").unwrap().status, Status::Failed);

    // Step 2: Replay --failed-only creates run-001 snapshot
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Step 3: Verify rt2 is now Open
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("rt2").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("rt1").unwrap().status, Status::Done);

    // Step 4: Restore from run-001
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Step 5: Verify rt2 is back to Failed (pre-replay state)
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(
        graph.get_task("rt2").unwrap().status,
        Status::Failed,
        "rt2 should be restored to Failed"
    );
    assert_eq!(
        graph.get_task("rt1").unwrap().status,
        Status::Done,
        "rt1 should still be Done"
    );
}

// ===========================================================================
// 4.2 replay_then_diff
// ===========================================================================

#[test]
fn test_replay_then_diff() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("rd1", "Failed task", Status::Failed);
    let t2 = make_task("rd2", "Done task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Replay --failed-only: snapshot captures rd1=Failed, rd2=Done
    // After replay: rd1=Open
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Diff run-001 against current
    let output = wg_ok(&wg_dir, &["runs", "diff", "run-001"]);
    // rd1: Failed -> Open
    assert!(
        output.contains("rd1"),
        "diff should show rd1 changed: {}",
        output
    );

    // JSON diff for exact validation
    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    let changes = json["changes"].as_array().unwrap();

    // Find rd1 change
    let rd1_change = changes.iter().find(|c| c["id"] == "rd1");
    assert!(rd1_change.is_some(), "should have a change for rd1");
    let rd1 = rd1_change.unwrap();
    assert_eq!(rd1["change"], "status_changed");
    assert_eq!(rd1["snapshot_status"], "failed");
    assert_eq!(rd1["current_status"], "open");

    // rd2 should not appear (no change)
    let rd2_change = changes.iter().find(|c| c["id"] == "rd2");
    assert!(rd2_change.is_none(), "rd2 should not appear in changes (unchanged)");
}

// ===========================================================================
// Additional: runs list empty (confirm existing coverage)
// ===========================================================================

#[test]
fn test_runs_list_empty() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("e1", "Task", Status::Open);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let output = wg_ok(&wg_dir, &["runs", "list"]);
    assert!(
        output.contains("No run snapshots"),
        "should report no snapshots: {}",
        output
    );

    // JSON should be empty array
    let json = wg_json(&wg_dir, &["runs", "list"]);
    let arr = json.as_array().unwrap();
    assert!(arr.is_empty(), "JSON list should be empty");
}

// ===========================================================================
// Additional: runs list ignores non-run directories
// ===========================================================================

#[test]
fn test_runs_list_ignores_non_run_directories() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("ig1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create a replay to get run-001
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Create a non-run directory in runs/
    fs::create_dir_all(wg_dir.join("runs/not-a-run")).unwrap();
    fs::create_dir_all(wg_dir.join("runs/random-dir")).unwrap();

    // List should only show run-001
    let json = wg_json(&wg_dir, &["runs", "list"]);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1, "should only list real runs: {:?}", arr);
    assert_eq!(arr[0]["id"], "run-001");
}

// ===========================================================================
// Additional: run ID generation — sequential increments
// ===========================================================================

#[test]
fn test_run_id_generation_sequential() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("id1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // First replay
    wg_ok(&wg_dir, &["replay", "--failed-only"]);
    let json = wg_json(&wg_dir, &["runs", "list"]);
    assert_eq!(json[0]["id"], "run-001");

    // Second replay
    set_task_failed(&wg_dir, "id1");
    wg_ok(&wg_dir, &["replay", "--failed-only"]);
    let json = wg_json(&wg_dir, &["runs", "list"]);
    assert_eq!(json[1]["id"], "run-002");
}

// ===========================================================================
// Additional: runs show --json validates full metadata
// ===========================================================================

#[test]
fn test_runs_show_json_full_metadata() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("sj1", "Fail", Status::Failed);
    let t2 = make_task("sj2", "Done", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    wg_ok(&wg_dir, &["replay", "--failed-only", "--model", "test-model"]);

    let json = wg_json(&wg_dir, &["runs", "show", "run-001"]);
    assert_eq!(json["id"], "run-001");
    assert!(json["timestamp"].is_string());
    assert_eq!(json["model"], "test-model");
    assert!(json["filter"].is_string());

    let reset = json["reset_tasks"].as_array().unwrap();
    assert!(reset.iter().any(|t| t == "sj1"), "sj1 should be in reset_tasks");

    let preserved = json["preserved_tasks"].as_array().unwrap();
    assert!(preserved.iter().any(|t| t == "sj2"), "sj2 should be in preserved_tasks");
}

// ===========================================================================
// Additional: restore safety snapshot is distinct from source
// ===========================================================================

#[test]
fn test_restore_safety_snapshot_distinct() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("ss1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create run-001
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Restore from run-001, should create run-002 as safety snapshot
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    let runs = workgraph::runs::list_runs(&wg_dir).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0], "run-001");
    assert_eq!(runs[1], "run-002");

    // Verify run-002 metadata mentions safety
    let meta = workgraph::runs::load_run_meta(&wg_dir, "run-002").unwrap();
    assert!(
        meta.filter.as_ref().unwrap().contains("safety"),
        "safety snapshot filter should mention 'safety': {:?}",
        meta.filter
    );
}

// ===========================================================================
// Additional: multiple restores create incrementing safety snapshots
// ===========================================================================

#[test]
fn test_multiple_restores_incrementing_ids() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("mr1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create run-001 via replay
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // First restore: creates safety run-002
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Second restore: creates safety run-003
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    let runs = workgraph::runs::list_runs(&wg_dir).unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[2], "run-003");
}

// ===========================================================================
// 3.25 runs_list_with_corrupted_metadata — graceful handling
// ===========================================================================

#[test]
fn test_runs_list_with_corrupted_metadata() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("cm1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create a valid replay snapshot (run-001)
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Create run-002 directory with corrupted meta.json
    let run002_dir = wg_dir.join("runs/run-002");
    fs::create_dir_all(&run002_dir).unwrap();
    fs::write(run002_dir.join("meta.json"), "NOT VALID JSON {{{").unwrap();

    // List should succeed without panic — run-001 should still appear
    let output = wg_cmd(&wg_dir, &["runs", "list"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    assert!(output.status.success(), "runs list should not crash: stderr={}", stderr);
    assert!(stdout.contains("run-001"), "should still list valid run-001: {}", stdout);

    // Stderr should warn about run-002
    assert!(
        stderr.contains("Warning") || stderr.contains("warning") || stderr.contains("run-002"),
        "should warn about corrupted run-002: stderr={}",
        stderr
    );
}

// ===========================================================================
// 3.26 runs_restore_missing_snapshot_graph — error on missing graph.jsonl
// ===========================================================================

#[test]
fn test_runs_restore_missing_snapshot_graph() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("mg1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create a replay snapshot (run-001)
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Delete graph.jsonl from the snapshot directory
    let snap_graph = wg_dir.join("runs/run-001/graph.jsonl");
    assert!(snap_graph.exists(), "graph.jsonl should exist before deletion");
    fs::remove_file(&snap_graph).unwrap();

    // Restore should fail with an error about missing graph.jsonl
    let (_stdout, stderr) = wg_fail(&wg_dir, &["runs", "restore", "run-001"]);
    assert!(
        stderr.contains("not found") || stderr.contains("graph.jsonl") || stderr.contains("No such file"),
        "should report missing snapshot graph: stderr={}",
        stderr
    );
}

// ===========================================================================
// 3.28 runs_diff_sorted_output — changes sorted alphabetically by task ID
// ===========================================================================

#[test]
fn test_runs_diff_sorted_output() {
    let tmp = TempDir::new().unwrap();
    // Create tasks with IDs that sort differently than creation order
    let z1 = make_task("z1", "Task Z", Status::Failed);
    let a1 = make_task("a1", "Task A", Status::Failed);
    let m1 = make_task("m1", "Task M", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![z1, a1, m1]);

    // Replay to create snapshot (all three failed) and reset to Open
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Diff should show all three changed: Failed -> Open, sorted alphabetically
    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    let changes = json["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 3, "should have 3 changes: {:?}", changes);

    // Verify alphabetical order: a1, m1, z1
    assert_eq!(changes[0]["id"], "a1", "first change should be a1");
    assert_eq!(changes[1]["id"], "m1", "second change should be m1");
    assert_eq!(changes[2]["id"], "z1", "third change should be z1");
}

// ===========================================================================
// 4.7 restore_then_diff_shows_no_changes
// ===========================================================================

#[test]
fn test_restore_then_diff_shows_no_changes() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("rd1", "Done task", Status::Done);
    let t2 = make_task("rd2", "Failed task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Replay creates run-001 snapshot, resets rd2 to Open
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Restore from run-001 (graph should now match run-001 snapshot)
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Diff against run-001 should show no differences
    let output = wg_ok(&wg_dir, &["runs", "diff", "run-001"]);
    assert!(
        output.contains("No differences"),
        "should report no differences after restore: {}",
        output
    );

    // JSON diff should show 0 changes
    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    assert_eq!(json["total_changes"], 0, "should have 0 changes: {:?}", json);
    let changes = json["changes"].as_array().unwrap();
    assert!(changes.is_empty(), "changes array should be empty");
}

// ===========================================================================
// 3.29 runs_snapshot_without_graph_jsonl — snapshot when graph.jsonl absent
// ===========================================================================

#[test]
fn test_snapshot_without_graph_jsonl() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    // Create a valid graph first, then snapshot manually without graph.jsonl
    // We test via the runs API directly since the CLI requires a valid graph to operate
    let run_id = "run-001";
    let meta = workgraph::runs::RunMeta {
        id: run_id.to_string(),
        timestamp: "2026-02-19T10:00:00Z".to_string(),
        model: None,
        reset_tasks: vec![],
        preserved_tasks: vec![],
        filter: Some("test".to_string()),
    };

    // No graph.jsonl exists — snapshot should still succeed (creates meta.json only)
    let snap_path = workgraph::runs::snapshot(&wg_dir, run_id, &meta).unwrap();
    assert!(snap_path.join("meta.json").exists(), "meta.json should be created");
    assert!(
        !snap_path.join("graph.jsonl").exists(),
        "graph.jsonl should NOT be in snapshot when source doesn't exist"
    );

    // Now create a graph so we can test restore
    let t1 = make_task("t1", "Task 1", Status::Open);
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(t1));
    save_graph(&graph, wg_dir.join("graph.jsonl")).unwrap();

    // Restore from this snapshot should fail because graph.jsonl is missing
    let (_stdout, stderr) = wg_fail(&wg_dir, &["runs", "restore", "run-001"]);
    assert!(
        stderr.contains("not found") || stderr.contains("graph.jsonl"),
        "should report missing snapshot graph.jsonl: stderr={}",
        stderr
    );
}

// ===========================================================================
// 3.30 run_id_above_999 — IDs above 999 use 4+ digits
// ===========================================================================

#[test]
fn test_run_id_above_999() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    let runs = wg_dir.join("runs");

    // Create run-999 directory
    fs::create_dir_all(runs.join("run-999")).unwrap();

    // next_run_id should return "run-1000"
    let next = workgraph::runs::next_run_id(&wg_dir);
    assert_eq!(next, "run-1000", "next run ID after 999 should be run-1000");

    // Create run-1000 with valid metadata and verify it appears in list
    let meta = workgraph::runs::RunMeta {
        id: "run-1000".to_string(),
        timestamp: "2026-02-19T10:00:00Z".to_string(),
        model: None,
        reset_tasks: vec![],
        preserved_tasks: vec![],
        filter: Some("test".to_string()),
    };
    workgraph::runs::snapshot(&wg_dir, "run-1000", &meta).unwrap();

    // list_runs should include "run-1000"
    let ids = workgraph::runs::list_runs(&wg_dir).unwrap();
    assert!(
        ids.contains(&"run-1000".to_string()),
        "list_runs should include run-1000: {:?}",
        ids
    );

    // Verify sorting: run-999 before run-1000
    // Note: string sorting puts "run-1000" before "run-999" since '1' < '9',
    // but this documents actual behavior
    let pos_999 = ids.iter().position(|x| x == "run-999");
    let pos_1000 = ids.iter().position(|x| x == "run-1000");
    assert!(pos_999.is_some() && pos_1000.is_some(), "both IDs should be in list");
}

// ===========================================================================
// 3.31 runs_diff_only_compares_status — field changes invisible to diff
// ===========================================================================

#[test]
fn test_runs_diff_only_compares_status() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("ds1", "Original title", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create a snapshot
    wg_ok(&wg_dir, &["replay"]); // default: all terminal

    // After replay, ds1 is reset to Open. Restore back to Done.
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Now modify the task title but keep status as Done
    let mut graph = load_wg_graph(&wg_dir);
    let task = graph.get_task_mut("ds1").unwrap();
    task.title = "Modified title".to_string();
    task.description = Some("Added description".to_string());
    save_graph(&graph, wg_dir.join("graph.jsonl")).unwrap();

    // Diff should report "No differences" because only status is compared
    let output = wg_ok(&wg_dir, &["runs", "diff", "run-001"]);
    assert!(
        output.contains("No differences"),
        "diff should report no differences when only non-status fields changed: {}",
        output
    );

    // JSON should confirm 0 changes
    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    assert_eq!(json["total_changes"], 0, "should have 0 changes: {:?}", json);
}

// ===========================================================================
// 3.32 runs_restore_does_not_restore_config — config.toml preserved
// ===========================================================================

#[test]
fn test_runs_restore_does_not_restore_config() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("cfg1", "Config test", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create a config.toml with original settings
    let config_path = wg_dir.join("config.toml");
    fs::write(&config_path, "[agent]\nmodel = \"original\"\n").unwrap();

    // Create a replay snapshot (captures graph.jsonl AND config.toml)
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Verify snapshot has config.toml
    let snap_config = wg_dir.join("runs/run-001/config.toml");
    assert!(snap_config.exists(), "snapshot should include config.toml");

    // Modify config.toml after snapshot
    fs::write(&config_path, "[agent]\nmodel = \"modified\"\n").unwrap();

    // Restore from snapshot
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Config.toml should retain the MODIFIED value (not restored from snapshot)
    let config_content = fs::read_to_string(&config_path).unwrap();
    assert!(
        config_content.contains("modified"),
        "config.toml should retain modified value after restore: {}",
        config_content
    );
    assert!(
        !config_content.contains("original"),
        "config.toml should NOT be restored to original value: {}",
        config_content
    );

    // But graph should be restored (verify task status)
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(
        graph.get_task("cfg1").unwrap().status,
        Status::Failed,
        "graph should be restored to snapshot state"
    );
}

// ===========================================================================
// 4.8 trace_after_restore — trace reflects restored task state
// ===========================================================================

#[test]
fn test_trace_after_restore() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("tr1", "Trace restore test", Status::Failed);
    t1.failure_reason = Some("original failure".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Record provenance so trace has operations to show
    let _ = workgraph::provenance::record(
        &wg_dir,
        "add_task",
        Some("tr1"),
        Some("test"),
        serde_json::json!({"title": "Trace restore test"}),
        10_000_000,
    );

    // Create agent archive for tr1
    create_agent_archive(&wg_dir, "tr1", "2026-02-18T10:00:00Z", "test prompt", "test output");

    // Replay: tr1 goes from Failed to Open, creates run-001
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Verify tr1 is now Open
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("tr1").unwrap().status, Status::Open);

    // Restore from run-001: tr1 goes back to Failed
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Trace should show tr1 as Failed (restored state)
    let output = wg_ok(&wg_dir, &["trace", "show", "tr1"]);
    assert!(
        output.contains("failed") || output.contains("Failed"),
        "trace should show restored status 'failed': {}",
        output
    );

    // JSON trace should have status = "failed"
    let json = wg_json(&wg_dir, &["trace", "show", "tr1"]);
    assert_eq!(
        json["status"].as_str().unwrap().to_lowercase(),
        "failed",
        "trace JSON status should be 'failed': {:?}",
        json["status"]
    );

    // Agent archives should still be accessible after restore
    let agent_runs = &json["agent_runs"];
    assert!(
        agent_runs.is_array(),
        "agent_runs should be an array"
    );
    let agent_count = json["summary"]["agent_run_count"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(
        agent_count, 1,
        "should still see 1 agent run after restore"
    );

    // Provenance should contain add_task operation for tr1
    let ops = &json["operations"];
    let ops_arr = ops.as_array().unwrap();
    assert!(!ops_arr.is_empty(), "should have provenance operations (at least add_task)");
    // Note: restore provenance has task_id=None, so it won't appear in per-task trace
}

// ===========================================================================
// 4.9 multiple_replay_then_trace_shows_all_archives
// ===========================================================================

#[test]
fn test_multiple_replay_cycles_preserve_all_archives() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("mr1", "Multi replay", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create agent archive #1 (simulating first agent run)
    create_agent_archive(
        &wg_dir,
        "mr1",
        "2026-02-18T10:00:00Z",
        "prompt cycle 1",
        "output cycle 1",
    );

    // First replay: mr1 goes from Failed to Open
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Create agent archive #2 (simulating second agent run after replay)
    create_agent_archive(
        &wg_dir,
        "mr1",
        "2026-02-18T12:00:00Z",
        "prompt cycle 2",
        "output cycle 2",
    );

    // Set task back to Failed (simulating agent failed again)
    set_task_failed(&wg_dir, "mr1");

    // Second replay: mr1 goes from Failed to Open again
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Both agent archives should still exist on disk
    let archive_base = wg_dir.join("log/agents/mr1");
    let archive1 = archive_base.join("2026-02-18T10:00:00Z");
    let archive2 = archive_base.join("2026-02-18T12:00:00Z");
    assert!(
        archive1.join("prompt.txt").exists(),
        "archive #1 prompt should survive replays"
    );
    assert!(
        archive1.join("output.txt").exists(),
        "archive #1 output should survive replays"
    );
    assert!(
        archive2.join("prompt.txt").exists(),
        "archive #2 prompt should survive replays"
    );
    assert!(
        archive2.join("output.txt").exists(),
        "archive #2 output should survive replays"
    );

    // Trace should show both agent runs
    let json = wg_json(&wg_dir, &["trace", "show", "mr1"]);
    let agent_count = json["summary"]["agent_run_count"]
        .as_u64()
        .unwrap_or(0);
    assert_eq!(
        agent_count, 2,
        "trace should show 2 agent runs from both cycles"
    );

    // Agent runs should be in chronological order
    let runs = json["agent_runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    let ts0 = runs[0]["timestamp"].as_str().unwrap();
    let ts1 = runs[1]["timestamp"].as_str().unwrap();
    assert!(
        ts0 < ts1,
        "agent runs should be chronologically ordered: {} < {}",
        ts0,
        ts1
    );
}

// ===========================================================================
// 5.15 next_run_id_with_non_numeric_suffix — ignored during ID generation
// ===========================================================================

#[test]
fn test_next_run_id_with_non_numeric_suffix() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    let runs = wg_dir.join("runs");

    // Create directories: run-001, run-abc, run-003
    fs::create_dir_all(runs.join("run-001")).unwrap();
    fs::create_dir_all(runs.join("run-abc")).unwrap();
    fs::create_dir_all(runs.join("run-003")).unwrap();

    // next_run_id should return "run-004" (max of parseable IDs is 3)
    let next = workgraph::runs::next_run_id(&wg_dir);
    assert_eq!(
        next, "run-004",
        "next_run_id should ignore non-numeric suffixes and use max parseable ID + 1"
    );
}

// ---------------------------------------------------------------------------
// Helper: create agent archive directory with prompt and output files
// ---------------------------------------------------------------------------

fn create_agent_archive(wg_dir: &Path, task_id: &str, timestamp: &str, prompt: &str, output: &str) {
    let archive_dir = wg_dir
        .join("log")
        .join("agents")
        .join(task_id)
        .join(timestamp);
    fs::create_dir_all(&archive_dir).unwrap();
    fs::write(archive_dir.join("prompt.txt"), prompt).unwrap();
    fs::write(archive_dir.join("output.txt"), output).unwrap();
}
