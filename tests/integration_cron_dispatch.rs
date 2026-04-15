//! Integration tests for cron task scheduling and coordinator dispatch.
//!
//! Covers: cron-due readiness gating, cron task reset after completion,
//! wg list --cron filter, wg edit --cron to set/clear cron schedules,
//! and library-level is_cron_due / reset_cron_task / is_time_ready integration.

use chrono::{DateTime, Duration, Utc};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::cron::reset_cron_task;
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::{is_time_ready, ready_tasks};

// ── helpers ──────────────────────────────────────────────────────────────

fn make_task(id: &str, title: &str) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        ..Task::default()
    }
}

fn setup_graph(tasks: Vec<Task>) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &path).unwrap();
    (dir, path)
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

// ── 1. Cron task becomes ready at fire time (library level) ─────────────

#[test]
fn cron_task_ready_when_due() {
    // Create a cron task with next_cron_fire in the past → should be ready
    let mut task = make_task("cron-ready", "Cron ready task");
    task.cron_enabled = true;
    task.cron_schedule = Some("0 0 2 * * *".to_string());
    // Set next_cron_fire to 1 hour ago
    task.next_cron_fire = Some((Utc::now() - Duration::hours(1)).to_rfc3339());

    let (_dir, path) = setup_graph(vec![task]);
    let graph = load_graph(&path).unwrap();

    let ready = ready_tasks(&graph);
    assert!(
        ready.iter().any(|t| t.id == "cron-ready"),
        "Cron task with past next_cron_fire should be ready"
    );
}

#[test]
fn cron_task_not_ready_before_fire_time() {
    // Create a cron task with next_cron_fire in the future → should NOT be ready
    let mut task = make_task("cron-future", "Cron future task");
    task.cron_enabled = true;
    task.cron_schedule = Some("0 0 2 * * *".to_string());
    task.next_cron_fire = Some((Utc::now() + Duration::hours(12)).to_rfc3339());

    let (_dir, path) = setup_graph(vec![task]);
    let graph = load_graph(&path).unwrap();

    let ready = ready_tasks(&graph);
    assert!(
        !ready.iter().any(|t| t.id == "cron-future"),
        "Cron task with future next_cron_fire should NOT be ready"
    );
}

#[test]
fn is_time_ready_gates_cron_tasks() {
    // Cron task with future fire time → not time-ready
    let mut future_task = make_task("cron-gate-future", "Future fire");
    future_task.cron_enabled = true;
    future_task.cron_schedule = Some("0 0 2 * * *".to_string());
    future_task.next_cron_fire = Some((Utc::now() + Duration::hours(6)).to_rfc3339());
    assert!(
        !is_time_ready(&future_task),
        "Cron task before fire time should not be time-ready"
    );

    // Cron task with past fire time → time-ready
    let mut past_task = make_task("cron-gate-past", "Past fire");
    past_task.cron_enabled = true;
    past_task.cron_schedule = Some("0 0 2 * * *".to_string());
    past_task.next_cron_fire = Some((Utc::now() - Duration::hours(1)).to_rfc3339());
    assert!(
        is_time_ready(&past_task),
        "Cron task after fire time should be time-ready"
    );

    // Non-cron task → time-ready (no gate)
    let normal_task = make_task("no-cron", "Normal task");
    assert!(
        is_time_ready(&normal_task),
        "Non-cron task should be time-ready"
    );
}

// ── 2. Cron task resets to Open after completion ────────────────────────

#[test]
fn cron_task_resets_after_done() {
    let mut task = Task {
        id: "cron-reset".to_string(),
        title: "Cron reset test".to_string(),
        status: Status::Done,
        cron_enabled: true,
        cron_schedule: Some("0 0 2 * * *".to_string()),
        assigned: Some("agent-1".to_string()),
        completed_at: Some(Utc::now().to_rfc3339()),
        ..Default::default()
    };

    let result = reset_cron_task(&mut task);
    assert!(result, "reset_cron_task should return true");
    assert_eq!(task.status, Status::Open, "Status should be Open");
    assert!(task.assigned.is_none(), "assigned should be cleared");
    assert!(
        task.completed_at.is_none(),
        "completed_at should be cleared"
    );
    assert!(
        task.last_cron_fire.is_some(),
        "last_cron_fire should be set"
    );
    assert!(
        task.next_cron_fire.is_some(),
        "next_cron_fire should be set for future dispatch"
    );

    // The next fire time should be in the future
    let next_fire: DateTime<Utc> = task
        .next_cron_fire
        .as_ref()
        .unwrap()
        .parse()
        .expect("next_cron_fire should be a valid timestamp");
    // Allow some jitter, but next_fire should be roughly within 24h+jitter from now
    assert!(
        next_fire > Utc::now() - Duration::minutes(15),
        "next_cron_fire should be approximately in the future (accounting for jitter)"
    );
}

// ── 3. wg list --cron shows only cron tasks (CLI level) ─────────────────

#[test]
fn list_cron_filter_shows_only_cron_tasks() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    // Add a cron task
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Cron task",
            "--id",
            "cron-1",
            "--cron",
            "0 0 2 * * *",
            "--immediate",
        ],
    );
    // Add a normal task
    wg_ok(
        &wg_dir,
        &["add", "Normal task", "--id", "normal-1", "--immediate"],
    );

    // List all tasks → both should appear
    let output_all = wg_ok(&wg_dir, &["list"]);
    assert!(
        output_all.contains("cron-1"),
        "All list should contain cron task"
    );
    assert!(
        output_all.contains("normal-1"),
        "All list should contain normal task"
    );

    // List with --cron → only cron task
    let output_cron = wg_ok(&wg_dir, &["list", "--cron"]);
    assert!(
        output_cron.contains("cron-1"),
        "Cron list should contain cron task. Output:\n{}",
        output_cron
    );
    assert!(
        !output_cron.contains("normal-1"),
        "Cron list should NOT contain normal task. Output:\n{}",
        output_cron
    );
}

#[test]
fn list_cron_filter_json() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Cron JSON",
            "--id",
            "cj1",
            "--cron",
            "0 */5 * * * *",
            "--immediate",
        ],
    );
    wg_ok(
        &wg_dir,
        &["add", "Normal JSON", "--id", "nj1", "--immediate"],
    );

    let output = wg_ok(&wg_dir, &["list", "--cron", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&output).expect("Should be valid JSON");
    let arr = json.as_array().expect("Should be array");

    assert_eq!(
        arr.len(),
        1,
        "Should have exactly 1 cron task in JSON output"
    );
    assert_eq!(arr[0]["id"], "cj1");
    assert_eq!(arr[0]["cron_enabled"], true);
    assert!(arr[0].get("cron_schedule").is_some());
}

// ── 4. wg edit --cron to set/clear cron schedule ────────────────────────

#[test]
fn edit_cron_set_schedule() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("e1", "Edit cron test")]);

    // Task should not be cron-enabled initially
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert!(!graph.get_task("e1").unwrap().cron_enabled);

    // Set cron schedule
    wg_ok(&wg_dir, &["edit", "e1", "--cron", "0 0 9 * * *"]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("e1").unwrap();
    assert!(
        task.cron_enabled,
        "cron_enabled should be true after edit --cron"
    );
    assert_eq!(
        task.cron_schedule.as_deref(),
        Some("0 0 9 * * *"),
        "cron_schedule should be set"
    );
    assert!(
        task.next_cron_fire.is_some(),
        "next_cron_fire should be computed"
    );
}

#[test]
fn edit_cron_clear_schedule() {
    let tmp = TempDir::new().unwrap();
    let mut cron_task = make_task("ec1", "Clear cron test");
    cron_task.cron_enabled = true;
    cron_task.cron_schedule = Some("0 0 2 * * *".to_string());
    cron_task.next_cron_fire = Some(Utc::now().to_rfc3339());
    let wg_dir = setup_workgraph(&tmp, vec![cron_task]);

    // Verify it's cron-enabled
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert!(graph.get_task("ec1").unwrap().cron_enabled);

    // Clear cron with empty string
    wg_ok(&wg_dir, &["edit", "ec1", "--cron", ""]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("ec1").unwrap();
    assert!(
        !task.cron_enabled,
        "cron_enabled should be false after clearing"
    );
    assert!(
        task.cron_schedule.is_none(),
        "cron_schedule should be None after clearing"
    );
    assert!(
        task.next_cron_fire.is_none(),
        "next_cron_fire should be None after clearing"
    );
}

#[test]
fn edit_cron_invalid_expression_fails() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("inv1", "Invalid cron")]);

    let output = wg_cmd(&wg_dir, &["edit", "inv1", "--cron", "not-a-cron"]);
    assert!(
        !output.status.success(),
        "Invalid cron expression should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Invalid cron expression"),
        "Error should mention invalid cron. Stderr:\n{}",
        stderr
    );
}

// ── 5. Integration: cron task in ready_tasks when due ───────────────────

#[test]
fn cron_task_appears_in_ready_when_due() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    // Create a cron task via CLI
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Cron ready test",
            "--id",
            "cr1",
            "--cron",
            "0 0 2 * * *",
            "--immediate",
        ],
    );

    // Manually set next_cron_fire to past so it's due
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task_mut("cr1").unwrap();
    task.next_cron_fire = Some((Utc::now() - Duration::hours(1)).to_rfc3339());
    save_graph(&graph, &graph_path).unwrap();

    // Verify it appears in ready_tasks
    let graph = load_graph(&graph_path).unwrap();
    let ready = ready_tasks(&graph);
    assert!(
        ready.iter().any(|t| t.id == "cr1"),
        "Cron task with past next_cron_fire should be in ready_tasks"
    );
}

#[test]
fn cron_task_not_in_ready_when_not_due() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Cron future test",
            "--id",
            "cf1",
            "--cron",
            "0 0 2 * * *",
            "--immediate",
        ],
    );

    // Set next_cron_fire to far future
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = load_graph(&graph_path).unwrap();
    let task = graph.get_task_mut("cf1").unwrap();
    task.next_cron_fire = Some((Utc::now() + Duration::hours(12)).to_rfc3339());
    save_graph(&graph, &graph_path).unwrap();

    let graph = load_graph(&graph_path).unwrap();
    let ready = ready_tasks(&graph);
    assert!(
        !ready.iter().any(|t| t.id == "cf1"),
        "Cron task with future next_cron_fire should NOT be in ready_tasks"
    );
}

// ── 6. Display: cron indicator in list output ───────────────────────────

#[test]
fn list_shows_cron_indicator() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Cron display",
            "--id",
            "cd1",
            "--cron",
            "0 0 2 * * *",
            "--immediate",
        ],
    );

    let output = wg_ok(&wg_dir, &["list"]);
    assert!(
        output.contains("[cron:"),
        "wg list should show [cron: ...] indicator for cron tasks. Output:\n{}",
        output
    );
}

// ── 7. Mixed: cron + dependencies ──────────────────────────────────────

#[test]
fn cron_task_with_dependency_not_ready_until_dep_done() {
    // Even if cron is due, task should not be ready if dependency is not done
    let dep = make_task("dep", "Dependency");
    let mut cron_task = make_task("cron-dep", "Cron with dep");
    cron_task.cron_enabled = true;
    cron_task.cron_schedule = Some("0 0 2 * * *".to_string());
    cron_task.next_cron_fire = Some((Utc::now() - Duration::hours(1)).to_rfc3339());
    cron_task.after = vec!["dep".to_string()];

    let (_dir, path) = setup_graph(vec![dep, cron_task]);
    let graph = load_graph(&path).unwrap();
    let ready = ready_tasks(&graph);

    // Only dep should be ready; cron-dep has unresolved dependency
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "dep");
}
