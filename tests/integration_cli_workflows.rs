//! Integration tests exercising CLI commands end-to-end.
//!
//! These tests invoke the real `wg` binary to verify command output
//! and state transitions for commonly-used commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{Estimate, Node, Status, Task, WorkGraph};
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

// ===========================================================================
// wg list
// ===========================================================================

#[test]
fn test_list_shows_all_tasks() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("t1", "First task", Status::Open),
            make_task("t2", "Second task", Status::InProgress),
            make_task("t3", "Third task", Status::Done),
        ],
    );

    let output = wg_ok(&wg_dir, &["list"]);
    assert!(output.contains("t1"));
    assert!(output.contains("t2"));
    assert!(output.contains("t3"));
}

#[test]
fn test_list_filter_by_status() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("open1", "Open task", Status::Open),
            make_task("done1", "Done task", Status::Done),
            make_task("ip1", "In-progress task", Status::InProgress),
        ],
    );

    let output = wg_ok(&wg_dir, &["list", "--status", "open"]);
    assert!(output.contains("open1"));
    assert!(!output.contains("done1"));
    assert!(!output.contains("ip1"));
}

#[test]
fn test_list_filter_done() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("open1", "Open task", Status::Open),
            make_task("done1", "Done task", Status::Done),
        ],
    );

    let output = wg_ok(&wg_dir, &["list", "--status", "done"]);
    assert!(output.contains("done1"));
    assert!(!output.contains("open1"));
}

#[test]
fn test_list_empty_graph() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_ok(&wg_dir, &["list"]);
    // Should succeed with empty output or a "no tasks" message
    assert!(output.is_empty() || output.contains("No tasks") || output.contains("0 task"));
}

#[test]
fn test_list_json_output() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("t1", "JSON task", Status::Open)]);

    let output = wg_ok(&wg_dir, &["list", "--json"]);
    // JSON output should be valid and contain the task
    let parsed: serde_json::Value = serde_json::from_str(&output)
        .unwrap_or_else(|e| panic!("Invalid JSON output: {}\nOutput: {}", e, output));
    assert!(parsed.is_array() || parsed.is_object());
}

#[test]
fn test_list_invalid_status_fails() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("t1", "Task", Status::Open)]);

    let output = wg_cmd(&wg_dir, &["list", "--status", "bogus"]);
    assert!(!output.status.success());
}

// ===========================================================================
// wg status
// ===========================================================================

#[test]
fn test_status_shows_summary() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("t1", "Open task", Status::Open),
            make_task("t2", "Done task", Status::Done),
            make_task("t3", "Failed task", Status::Failed),
        ],
    );

    let output = wg_ok(&wg_dir, &["status"]);
    // Status should show counts
    assert!(!output.is_empty());
}

#[test]
fn test_status_empty_graph() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_ok(&wg_dir, &["status"]);
    assert!(!output.is_empty());
}

// ===========================================================================
// wg check
// ===========================================================================

#[test]
fn test_check_clean_graph() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("t1", "Task 1", Status::Open),
            make_task("t2", "Task 2", Status::Open),
        ],
    );

    let output = wg_cmd(&wg_dir, &["check"]);
    assert!(output.status.success());
}

#[test]
fn test_check_detects_orphan_blockers() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    let mut task = make_task("t1", "Broken dep", Status::Open);
    task.blocked_by.push("nonexistent".to_string());

    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path).unwrap();

    let output = wg_cmd(&wg_dir, &["check"]);
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let combined = format!("{}{}", stdout, stderr);
    // Should detect the orphan reference
    assert!(
        combined.contains("nonexistent") || combined.contains("orphan") || !output.status.success(),
        "check should detect orphan blocker, got: {}",
        combined
    );
}

// ===========================================================================
// wg ready
// ===========================================================================

#[test]
fn test_ready_shows_unblocked_tasks() {
    let tmp = TempDir::new().unwrap();
    let mut blocked = make_task("child", "Blocked", Status::Open);
    blocked.blocked_by.push("parent".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("parent", "Parent", Status::Open), blocked],
    );

    let output = wg_ok(&wg_dir, &["ready"]);
    assert!(output.contains("parent"));
    assert!(!output.contains("child"));
}

#[test]
fn test_ready_after_dep_done() {
    let tmp = TempDir::new().unwrap();
    let mut blocked = make_task("child", "Blocked", Status::Open);
    blocked.blocked_by.push("parent".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("parent", "Parent", Status::Done), blocked],
    );

    let output = wg_ok(&wg_dir, &["ready"]);
    assert!(output.contains("child"));
}

// ===========================================================================
// wg show
// ===========================================================================

#[test]
fn test_show_displays_task_details() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("my-task", "Detailed task", Status::Open)],
    );

    let output = wg_ok(&wg_dir, &["show", "my-task"]);
    assert!(output.contains("my-task"));
    assert!(output.contains("Detailed task"));
}

#[test]
fn test_show_nonexistent_task_fails() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(&wg_dir, &["show", "ghost"]);
    assert!(!output.status.success());
}

#[test]
fn test_show_json_output() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("jt", "JSON task", Status::Open)]);

    let output = wg_ok(&wg_dir, &["show", "jt", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&output)
        .unwrap_or_else(|e| panic!("Invalid JSON: {}\nOutput: {}", e, output));
    assert!(parsed.is_object());
}

// ===========================================================================
// wg claim / unclaim via CLI
// ===========================================================================

#[test]
fn test_claim_via_cli() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("c1", "Claimable", Status::Open)]);

    wg_ok(&wg_dir, &["claim", "c1", "--actor", "agent-1"]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("c1").unwrap();
    assert_eq!(task.status, Status::InProgress);
    assert_eq!(task.assigned.as_deref(), Some("agent-1"));
}

#[test]
fn test_claim_done_task_fails_via_cli() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("c2", "Already done", Status::Done)]);

    let output = wg_cmd(&wg_dir, &["claim", "c2"]);
    assert!(!output.status.success());
}

#[test]
fn test_unclaim_via_cli() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![{
            let mut t = make_task("u1", "Unclaim me", Status::InProgress);
            t.assigned = Some("agent-1".to_string());
            t
        }],
    );

    wg_ok(&wg_dir, &["unclaim", "u1"]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("u1").unwrap();
    assert_eq!(task.status, Status::Open);
    assert!(task.assigned.is_none());
}

// ===========================================================================
// wg done via CLI
// ===========================================================================

#[test]
fn test_done_via_cli() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("d1", "Finish me", Status::InProgress)]);

    wg_ok(&wg_dir, &["done", "d1"]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("d1").unwrap();
    assert_eq!(task.status, Status::Done);
    assert!(task.completed_at.is_some());
}

#[test]
fn test_done_blocked_task_fails() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("d2", "Blocked done", Status::InProgress);
    task.blocked_by.push("blocker".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("blocker", "Blocker", Status::Open), task],
    );

    let output = wg_cmd(&wg_dir, &["done", "d2"]);
    assert!(!output.status.success());
}

// ===========================================================================
// wg fail / retry lifecycle via CLI
// ===========================================================================

#[test]
fn test_fail_retry_lifecycle_via_cli() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("fr1", "Fail and retry", Status::InProgress)],
    );

    // Fail the task
    wg_ok(&wg_dir, &["fail", "fr1", "--reason", "transient error"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("fr1").unwrap();
    assert_eq!(task.status, Status::Failed);

    // Retry the task
    wg_ok(&wg_dir, &["retry", "fr1"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("fr1").unwrap();
    assert_eq!(task.status, Status::Open);

    // Claim again
    wg_ok(&wg_dir, &["claim", "fr1"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("fr1").unwrap();
    assert_eq!(task.status, Status::InProgress);

    // Complete successfully
    wg_ok(&wg_dir, &["done", "fr1"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("fr1").unwrap();
    assert_eq!(task.status, Status::Done);
}

// ===========================================================================
// wg add via CLI
// ===========================================================================

#[test]
fn test_add_creates_task() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "New task", "--id", "new-1"]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("new-1").unwrap();
    assert_eq!(task.title, "New task");
    assert_eq!(task.status, Status::Open);
}

#[test]
fn test_add_with_dependencies() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("dep1", "Dependency", Status::Open)]);

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Dependent task",
            "--id",
            "child1",
            "--blocked-by",
            "dep1",
        ],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("child1").unwrap();
    assert!(task.blocked_by.contains(&"dep1".to_string()));
}

// ===========================================================================
// wg archive via CLI
// ===========================================================================

#[test]
fn test_archive_removes_done_tasks() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("open1", "Open task", Status::Open),
            make_task("done1", "Done task", Status::Done),
            make_task("done2", "Another done", Status::Done),
        ],
    );

    wg_ok(&wg_dir, &["archive"]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert!(graph.get_task("open1").is_some());
    assert!(graph.get_task("done1").is_none());
    assert!(graph.get_task("done2").is_none());
}

// ===========================================================================
// wg analyze via CLI
// ===========================================================================

#[test]
fn test_analyze_runs_on_graph() {
    let tmp = TempDir::new().unwrap();
    let mut blocked = make_task("child", "Blocked", Status::Open);
    blocked.blocked_by.push("parent".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("parent", "Parent task", Status::Open),
            blocked,
            make_task("done1", "Done", Status::Done),
        ],
    );

    let output = wg_ok(&wg_dir, &["analyze"]);
    assert!(!output.is_empty());
}

// ===========================================================================
// wg edit via CLI
// ===========================================================================

#[test]
fn test_edit_title() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("e1", "Old title", Status::Open)]);

    wg_ok(&wg_dir, &["edit", "e1", "--title", "New title"]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("e1").unwrap();
    assert_eq!(task.title, "New title");
}

#[test]
fn test_edit_description() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("e2", "Edit desc", Status::Open)]);

    wg_ok(&wg_dir, &["edit", "e2", "-d", "New description"]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("e2").unwrap();
    assert_eq!(task.description.as_deref(), Some("New description"));
}

#[test]
fn test_edit_nonexistent_task_fails() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(&wg_dir, &["edit", "ghost", "--title", "X"]);
    assert!(!output.status.success());
}

// ===========================================================================
// wg log via CLI
// ===========================================================================

#[test]
fn test_log_adds_entry() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("l1", "Logged task", Status::InProgress)],
    );

    wg_ok(&wg_dir, &["log", "l1", "Working on implementation"]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("l1").unwrap();
    assert!(
        task.log
            .iter()
            .any(|e| e.message.contains("Working on implementation"))
    );
}

// ===========================================================================
// wg abandon via CLI
// ===========================================================================

#[test]
fn test_abandon_task() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("a1", "Abandon me", Status::InProgress)],
    );

    wg_ok(&wg_dir, &["abandon", "a1", "--reason", "no longer needed"]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("a1").unwrap();
    assert_eq!(task.status, Status::Abandoned);
}

// ===========================================================================
// wg why-blocked via CLI
// ===========================================================================

#[test]
fn test_why_blocked_shows_blockers() {
    let tmp = TempDir::new().unwrap();
    let mut blocked = make_task("child", "Blocked task", Status::Blocked);
    blocked.blocked_by.push("parent".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("parent", "Blocker", Status::Open), blocked],
    );

    let output = wg_ok(&wg_dir, &["why-blocked", "child"]);
    assert!(output.contains("parent"));
}

// ===========================================================================
// wg blocked via CLI
// ===========================================================================

#[test]
fn test_blocked_shows_blockers_of_task() {
    let tmp = TempDir::new().unwrap();
    let mut child = make_task("child", "Blocked child", Status::Open);
    child.blocked_by.push("parent".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("parent", "Parent", Status::Open), child],
    );

    let output = wg_ok(&wg_dir, &["blocked", "child"]);
    assert!(output.contains("parent"));
}

// ===========================================================================
// wg retry lifecycle (fail → retry → claim → done)
// ===========================================================================

#[test]
fn test_retry_lifecycle_fail_retry_claim_done() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("t1", "Retry test", Status::Open)]);

    // Claim the task
    wg_ok(&wg_dir, &["claim", "t1"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("t1").unwrap().status, Status::InProgress);

    // Fail the task
    wg_ok(&wg_dir, &["fail", "t1", "--reason", "timeout"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Failed);

    // Retry the task
    wg_ok(&wg_dir, &["retry", "t1"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("t1").unwrap();
    assert_eq!(task.status, Status::Open);
    assert_eq!(task.retry_count, 1);

    // Claim again and complete
    wg_ok(&wg_dir, &["claim", "t1"]);
    wg_ok(&wg_dir, &["done", "t1"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Done);
}

#[test]
fn test_retry_respects_max_retries() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("t1", "Max retry test", Status::Failed);
    task.retry_count = 3;
    task.max_retries = Some(3);
    task.failure_reason = Some("error".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![task]);

    let output = wg_cmd(&wg_dir, &["retry", "t1"]);
    assert!(
        !output.status.success(),
        "retry should fail when max_retries exceeded"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("maximum") || stderr.contains("retries"),
        "stderr should mention max retries: {}",
        stderr
    );
}

// ===========================================================================
// wg status
// ===========================================================================

#[test]
fn test_status_empty_graph_cli() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_ok(&wg_dir, &["status"]);
    // Should not panic on empty graph
    assert!(output.contains("0") || output.contains("empty") || output.contains("No tasks"));
}

#[test]
fn test_status_json_output() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("t1", "Task A", Status::Open),
            make_task("t2", "Task B", Status::Done),
        ],
    );

    let output = wg_ok(&wg_dir, &["status", "--json"]);
    // JSON output should be valid
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap_or_else(|e| {
        panic!(
            "status --json should produce valid JSON: {}\nOutput: {}",
            e, output
        )
    });
    assert!(parsed.is_object());
}

// ===========================================================================
// wg show edge cases
// ===========================================================================

#[test]
fn test_show_task_with_all_fields_populated() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("t1", "Full task", Status::InProgress);
    task.description = Some("A detailed description".to_string());
    task.assigned = Some("agent-1".to_string());
    task.tags = vec!["urgent".to_string(), "backend".to_string()];
    task.skills = vec!["rust".to_string()];
    task.blocked_by = vec!["t0".to_string()];
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("t0", "Prerequisite", Status::Done), task],
    );

    let output = wg_ok(&wg_dir, &["show", "t1"]);
    assert!(output.contains("Full task"));
    assert!(output.contains("agent-1"));
}

#[test]
fn test_show_json_output_with_id() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("t1", "JSON test", Status::Open)]);

    let output = wg_ok(&wg_dir, &["show", "t1", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap_or_else(|e| {
        panic!(
            "show --json should produce valid JSON: {}\nOutput: {}",
            e, output
        )
    });
    assert!(parsed.is_object());
    assert_eq!(parsed["id"], "t1");
}

// ===========================================================================
// wg check
// ===========================================================================

#[test]
fn test_check_clean_graph_no_errors() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("t1", "Task A", Status::Open),
            make_task("t2", "Task B", Status::Done),
        ],
    );

    let output = wg_ok(&wg_dir, &["check"]);
    // Clean graph should not report errors
    assert!(
        !output.contains("ERROR") && !output.contains("error"),
        "clean graph should not show errors: {}",
        output
    );
}

// ===========================================================================
// wg ready
// ===========================================================================

#[test]
fn test_ready_excludes_blocked_tasks() {
    let tmp = TempDir::new().unwrap();
    let mut blocked = make_task("blocked", "Blocked task", Status::Open);
    blocked.blocked_by.push("blocker".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("blocker", "Blocker", Status::Open), blocked],
    );

    let output = wg_ok(&wg_dir, &["ready"]);
    assert!(
        output.contains("blocker"),
        "ready should show unblocked task"
    );
    assert!(
        !output.contains("Blocked task"),
        "ready should not show blocked task"
    );
}

#[test]
fn test_cost_json_output() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("ct", "Cost task", Status::Open);
    task.estimate = Some(Estimate {
        hours: Some(5.0),
        cost: Some(250.0),
    });
    let wg_dir = setup_workgraph(&tmp, vec![task]);

    let output = wg_ok(&wg_dir, &["cost", "ct", "--json"]);
    let parsed: serde_json::Value = serde_json::from_str(&output)
        .unwrap_or_else(|e| panic!("Invalid JSON: {}\nOutput: {}", e, output));
    assert_eq!(parsed["task_id"], "ct");
    assert_eq!(parsed["total_cost"], 250.0);
}

// ===========================================================================
// JSON output parsing tests — structured output for machine consumption
// ===========================================================================

/// Helper: parse JSON from wg command output, panic with diagnostics on failure.
fn parse_json(output: &str, cmd: &str) -> serde_json::Value {
    serde_json::from_str(output).unwrap_or_else(|e| {
        panic!(
            "wg {} --json produced invalid JSON: {}\nOutput: {}",
            cmd, e, output
        )
    })
}

// ── wg list --json ──────────────────────────────────────────────────

#[test]
fn test_list_json_fields() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Open task", Status::Open);
    t1.blocked_by = vec!["t2".to_string()];
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            t1,
            make_task("t2", "Done task", Status::Done),
            make_task("t3", "In-progress", Status::InProgress),
        ],
    );

    let output = wg_ok(&wg_dir, &["list", "--json"]);
    let parsed = parse_json(&output, "list");
    let arr = parsed.as_array().expect("list --json should be an array");
    assert_eq!(arr.len(), 3);

    // Each entry must have id, title, status, blocked_by
    for item in arr {
        assert!(item["id"].is_string(), "id should be a string");
        assert!(item["title"].is_string(), "title should be a string");
        assert!(item["status"].is_string(), "status should be a string");
        assert!(
            item["blocked_by"].is_array(),
            "blocked_by should be an array"
        );
    }

    // Verify specific values
    let t1_item = arr.iter().find(|i| i["id"] == "t1").expect("t1 in list");
    assert_eq!(t1_item["title"], "Open task");
    assert_eq!(t1_item["status"], "open");
    assert_eq!(t1_item["blocked_by"][0], "t2");
}

#[test]
fn test_list_json_with_status_filter() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("o1", "Open", Status::Open),
            make_task("d1", "Done", Status::Done),
            make_task("ip1", "In progress", Status::InProgress),
        ],
    );

    let output = wg_ok(&wg_dir, &["list", "--status", "done", "--json"]);
    let parsed = parse_json(&output, "list --status done");
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "d1");
    assert_eq!(arr[0]["status"], "done");
}

#[test]
fn test_list_json_empty_graph() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_ok(&wg_dir, &["list", "--json"]);
    let parsed = parse_json(&output, "list");
    let arr = parsed.as_array().unwrap();
    assert!(arr.is_empty());
}

// ── wg show --json ──────────────────────────────────────────────────

#[test]
fn test_show_json_fields() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("st1", "Show task", Status::InProgress);
    task.description = Some("Detailed description".to_string());
    task.assigned = Some("agent-42".to_string());
    task.tags = vec!["backend".to_string(), "urgent".to_string()];
    task.skills = vec!["rust".to_string()];
    task.blocked_by = vec!["dep1".to_string()];
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("dep1", "Dependency", Status::Done), task],
    );

    let output = wg_ok(&wg_dir, &["show", "st1", "--json"]);
    let parsed = parse_json(&output, "show");
    assert!(parsed.is_object(), "show --json should be an object");

    // Required fields
    assert_eq!(parsed["id"], "st1");
    assert_eq!(parsed["title"], "Show task");
    assert_eq!(parsed["status"], "in-progress");
    assert_eq!(parsed["description"], "Detailed description");
    assert_eq!(parsed["assigned"], "agent-42");

    // Array fields
    let tags = parsed["tags"].as_array().unwrap();
    assert!(tags.contains(&serde_json::json!("backend")));
    assert!(tags.contains(&serde_json::json!("urgent")));

    let skills = parsed["skills"].as_array().unwrap();
    assert!(skills.contains(&serde_json::json!("rust")));

    // blocked_by should be objects with id/title/status
    let blocked_by = parsed["blocked_by"].as_array().unwrap();
    assert_eq!(blocked_by.len(), 1);
    assert_eq!(blocked_by[0]["id"], "dep1");
}

#[test]
fn test_show_json_minimal_task() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("m1", "Minimal", Status::Open)]);

    let output = wg_ok(&wg_dir, &["show", "m1", "--json"]);
    let parsed = parse_json(&output, "show");
    assert_eq!(parsed["id"], "m1");
    assert_eq!(parsed["title"], "Minimal");
    assert_eq!(parsed["status"], "open");

    // Optional fields should be absent (skip_serializing_if)
    assert!(parsed.get("description").is_none() || parsed["description"].is_null());
    assert!(parsed.get("assigned").is_none() || parsed["assigned"].is_null());
}

// ── wg ready --json ─────────────────────────────────────────────────

#[test]
fn test_ready_json_fields() {
    let tmp = TempDir::new().unwrap();
    let mut blocked = make_task("child", "Blocked", Status::Open);
    blocked.blocked_by.push("parent".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("parent", "Parent", Status::Open), blocked],
    );

    let output = wg_ok(&wg_dir, &["ready", "--json"]);
    let parsed = parse_json(&output, "ready");
    let arr = parsed.as_array().expect("ready --json should be an array");

    // Only parent should be ready
    assert_eq!(arr.len(), 1);
    let item = &arr[0];
    assert_eq!(item["id"], "parent");
    assert_eq!(item["title"], "Parent");
    assert_eq!(item["ready"], true);
}

#[test]
fn test_ready_json_empty() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("done1", "Already done", Status::Done)],
    );

    let output = wg_ok(&wg_dir, &["ready", "--json"]);
    let parsed = parse_json(&output, "ready");
    let arr = parsed.as_array().unwrap();
    assert!(arr.is_empty());
}

#[test]
fn test_ready_json_with_assigned() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("r1", "Ready assigned", Status::Open);
    task.assigned = Some("agent-7".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![task]);

    let output = wg_ok(&wg_dir, &["ready", "--json"]);
    let parsed = parse_json(&output, "ready");
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["assigned"], "agent-7");
}

// ── wg blocked --json ───────────────────────────────────────────────

#[test]
fn test_blocked_json_fields() {
    let tmp = TempDir::new().unwrap();
    let mut child = make_task("child", "Blocked child", Status::Open);
    child.blocked_by.push("b1".to_string());
    child.blocked_by.push("b2".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("b1", "Blocker one", Status::Open),
            make_task("b2", "Blocker two", Status::InProgress),
            child,
        ],
    );

    let output = wg_ok(&wg_dir, &["blocked", "child", "--json"]);
    let parsed = parse_json(&output, "blocked");
    let arr = parsed.as_array().expect("blocked --json should be an array");
    assert_eq!(arr.len(), 2);

    // Each blocker must have id, title, status
    for item in arr {
        assert!(item["id"].is_string());
        assert!(item["title"].is_string());
        assert!(item["status"].is_string());
    }

    let ids: Vec<&str> = arr.iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"b1"));
    assert!(ids.contains(&"b2"));
}

#[test]
fn test_blocked_json_no_blockers() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("t1", "Free task", Status::Open)]);

    let output = wg_ok(&wg_dir, &["blocked", "t1", "--json"]);
    let parsed = parse_json(&output, "blocked");
    let arr = parsed.as_array().unwrap();
    assert!(arr.is_empty());
}

#[test]
fn test_blocked_json_excludes_done_blockers() {
    let tmp = TempDir::new().unwrap();
    let mut child = make_task("child", "Blocked", Status::Open);
    child.blocked_by.push("done-dep".to_string());
    child.blocked_by.push("open-dep".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("done-dep", "Done dep", Status::Done),
            make_task("open-dep", "Open dep", Status::Open),
            child,
        ],
    );

    let output = wg_ok(&wg_dir, &["blocked", "child", "--json"]);
    let parsed = parse_json(&output, "blocked");
    let arr = parsed.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "open-dep");
}

// ── wg why-blocked --json ───────────────────────────────────────────

#[test]
fn test_why_blocked_json_fields() {
    let tmp = TempDir::new().unwrap();
    let mut child = make_task("child", "Blocked child", Status::Blocked);
    child.blocked_by.push("parent".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("parent", "Blocker", Status::Open), child],
    );

    let output = wg_ok(&wg_dir, &["why-blocked", "child", "--json"]);
    let parsed = parse_json(&output, "why-blocked");
    assert!(parsed.is_object());
    assert!(parsed.get("task").is_some() || parsed.get("task_id").is_some());
    assert!(
        parsed.get("root_blockers").is_some() || parsed.get("blocking_chain").is_some(),
        "why-blocked should have root_blockers or blocking_chain"
    );
}

// ── wg context --json ───────────────────────────────────────────────

#[test]
fn test_context_json_fields() {
    let tmp = TempDir::new().unwrap();
    let mut dep = make_task("dep", "Dependency", Status::Done);
    dep.artifacts = vec!["output.txt".to_string()];
    let mut child = make_task("child", "Dependent", Status::Open);
    child.blocked_by.push("dep".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![dep, child]);

    let output = wg_ok(&wg_dir, &["context", "child", "--json"]);
    let parsed = parse_json(&output, "context");
    assert!(parsed.is_object());
    assert_eq!(parsed["task_id"], "child");

    // Should have available_context with dep's artifacts
    let ctx = parsed["available_context"].as_array().unwrap();
    assert_eq!(ctx.len(), 1);
    assert_eq!(ctx[0]["task_id"], "dep");
    assert!(ctx[0]["artifacts"].as_array().unwrap().contains(&serde_json::json!("output.txt")));
}

#[test]
fn test_context_json_no_deps() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("solo", "Solo task", Status::Open)]);

    let output = wg_ok(&wg_dir, &["context", "solo", "--json"]);
    let parsed = parse_json(&output, "context");
    assert_eq!(parsed["task_id"], "solo");
    let ctx = parsed["available_context"].as_array().unwrap();
    assert!(ctx.is_empty());
}

// ── wg loops --json ─────────────────────────────────────────────────

#[test]
fn test_loops_json_clean_graph() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("t1", "Task 1", Status::Open),
            make_task("t2", "Task 2", Status::Open),
        ],
    );

    let output = wg_ok(&wg_dir, &["loops", "--json"]);
    let parsed = parse_json(&output, "loops");
    assert!(parsed.is_object());
    assert_eq!(parsed["cycles_detected"], 0);
    assert!(parsed["cycles"].as_array().unwrap().is_empty());
    assert!(parsed["loop_edges"].as_array().unwrap().is_empty());
}

// ── wg check --json ─────────────────────────────────────────────────

#[test]
fn test_check_json_clean_graph() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("t1", "Task 1", Status::Open),
            make_task("t2", "Task 2", Status::Done),
        ],
    );

    let output = wg_ok(&wg_dir, &["check", "--json"]);
    let parsed = parse_json(&output, "check");
    assert!(parsed.is_object());
    assert_eq!(parsed["ok"], true);
}

#[test]
fn test_check_json_with_orphan_refs() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    std::fs::create_dir_all(&wg_dir).unwrap();

    let mut task = make_task("t1", "Broken", Status::Open);
    task.blocked_by.push("nonexistent".to_string());

    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path).unwrap();

    let output = wg_cmd(&wg_dir, &["check", "--json"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    // check may exit non-zero for bad graph, but JSON should still be valid
    if !stdout.trim().is_empty() {
        let parsed = parse_json(&stdout, "check");
        assert!(parsed.is_object());
        // Should report the issue
        assert!(
            parsed["ok"] == false
                || parsed
                    .get("orphan_refs")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false),
            "check should report orphan refs"
        );
    }
}

// ── wg service status --json (no service running) ───────────────────

#[test]
fn test_service_status_json_not_running() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("t1", "Task", Status::Open)]);

    let output = wg_ok(&wg_dir, &["service", "status", "--json"]);
    let parsed = parse_json(&output, "service status");
    assert!(parsed.is_object());
    assert_eq!(parsed["status"], "not_running");
}

// ── wg status --json ────────────────────────────────────────────────

#[test]
fn test_status_json_fields() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("t1", "Open", Status::Open),
            make_task("t2", "Done", Status::Done),
            make_task("t3", "Failed", Status::Failed),
            make_task("t4", "InProgress", Status::InProgress),
        ],
    );

    let output = wg_ok(&wg_dir, &["status", "--json"]);
    let parsed = parse_json(&output, "status");
    assert!(parsed.is_object());
    // status should contain task counts
    assert!(
        parsed.get("tasks").is_some() || parsed.get("total").is_some(),
        "status --json should contain task information: {}",
        output
    );
}

// ── wg analyze --json ───────────────────────────────────────────────

#[test]
fn test_analyze_json_output() {
    let tmp = TempDir::new().unwrap();
    let mut blocked = make_task("child", "Blocked", Status::Open);
    blocked.blocked_by.push("parent".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("parent", "Parent", Status::Open), blocked],
    );

    let output = wg_ok(&wg_dir, &["analyze", "--json"]);
    let parsed = parse_json(&output, "analyze");
    assert!(parsed.is_object());
}

// ── wg structure --json ─────────────────────────────────────────────

#[test]
fn test_structure_json_output() {
    let tmp = TempDir::new().unwrap();
    let mut child = make_task("child", "Child", Status::Open);
    child.blocked_by.push("parent".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("parent", "Parent", Status::Open), child],
    );

    let output = wg_ok(&wg_dir, &["structure", "--json"]);
    let parsed = parse_json(&output, "structure");
    assert!(parsed.is_object() || parsed.is_array());
}

// ── wg bottlenecks --json ───────────────────────────────────────────

#[test]
fn test_bottlenecks_json_output() {
    let tmp = TempDir::new().unwrap();
    let mut c1 = make_task("c1", "Child 1", Status::Open);
    c1.blocked_by.push("hub".to_string());
    let mut c2 = make_task("c2", "Child 2", Status::Open);
    c2.blocked_by.push("hub".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("hub", "Hub task", Status::Open), c1, c2],
    );

    let output = wg_ok(&wg_dir, &["bottlenecks", "--json"]);
    let parsed = parse_json(&output, "bottlenecks");
    assert!(parsed.is_object() || parsed.is_array());
}

// ── wg critical-path --json ─────────────────────────────────────────

#[test]
fn test_critical_path_json_output() {
    let tmp = TempDir::new().unwrap();
    let mut child = make_task("child", "Child", Status::Open);
    child.blocked_by.push("parent".to_string());
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("parent", "Parent", Status::Open), child],
    );

    let output = wg_ok(&wg_dir, &["critical-path", "--json"]);
    let parsed = parse_json(&output, "critical-path");
    assert!(parsed.is_object() || parsed.is_array());
}

// ── wg log --list --json ────────────────────────────────────────────

#[test]
fn test_log_list_json_output() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![make_task("lt", "Logged task", Status::InProgress)],
    );

    // Add a log entry first
    wg_ok(&wg_dir, &["log", "lt", "Test log message"]);

    let output = wg_ok(&wg_dir, &["log", "lt", "--list", "--json"]);
    let parsed = parse_json(&output, "log --list");
    let arr = parsed.as_array().expect("log --list --json should be an array");
    assert!(!arr.is_empty());
    assert!(arr[0]["message"].is_string());
    assert!(arr[0]["timestamp"].is_string());
}

// ── wg agents --json (no agents running) ────────────────────────────

#[test]
fn test_agents_json_empty() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("t1", "Task", Status::Open)]);

    let output = wg_ok(&wg_dir, &["agents", "--json"]);
    let parsed = parse_json(&output, "agents");
    let arr = parsed.as_array().expect("agents --json should be an array");
    assert!(arr.is_empty());
}

// ── wg archive --list --json ────────────────────────────────────────

#[test]
fn test_archive_list_json_output() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("open1", "Open", Status::Open),
            make_task("done1", "Done", Status::Done),
        ],
    );

    let output = wg_ok(&wg_dir, &["archive", "--list", "--json"]);
    let parsed = parse_json(&output, "archive --list");
    // Should list tasks that would be archived
    assert!(parsed.is_object() || parsed.is_array());
}

// ── JSON output stability: round-trip parsing ───────────────────────

#[test]
fn test_list_json_round_trip() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(
        &tmp,
        vec![
            make_task("a", "Alpha", Status::Open),
            make_task("b", "Beta", Status::Done),
        ],
    );

    let output = wg_ok(&wg_dir, &["list", "--json"]);
    // Parse to Value, serialize back, parse again — should be stable
    let v1: serde_json::Value = serde_json::from_str(&output).unwrap();
    let serialized = serde_json::to_string(&v1).unwrap();
    let v2: serde_json::Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(v1, v2);
}

#[test]
fn test_show_json_round_trip() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("rt", "Round trip", Status::Open)]);

    let output = wg_ok(&wg_dir, &["show", "rt", "--json"]);
    let v1: serde_json::Value = serde_json::from_str(&output).unwrap();
    let serialized = serde_json::to_string(&v1).unwrap();
    let v2: serde_json::Value = serde_json::from_str(&serialized).unwrap();
    assert_eq!(v1, v2);
}

// ── Edge cases: --json with empty/error states ──────────────────────

#[test]
fn test_show_json_nonexistent_fails_gracefully() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(&wg_dir, &["show", "ghost", "--json"]);
    assert!(
        !output.status.success(),
        "show of nonexistent task should fail"
    );
}

#[test]
fn test_blocked_json_nonexistent_fails_gracefully() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(&wg_dir, &["blocked", "ghost", "--json"]);
    assert!(
        !output.status.success(),
        "blocked of nonexistent task should fail"
    );
}
