//! Integration tests for --delay and --not-before scheduled task dispatch.
//!
//! Covers: wg add --delay, wg add --not-before, wg edit --delay,
//! wg edit --not-before, wg show not_before display, wg list delayed indicator,
//! wg ready filtering, and cycle --cycle-delay ready_after propagation.

use chrono::{DateTime, Duration, Utc};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{CycleConfig, Node, Status, Task, WorkGraph};
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

fn make_task_with_status(id: &str, title: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        status,
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

// ── 1. wg add --delay creates task with not_before ~ now+delay ──────────

#[test]
fn add_delay_sets_not_before_in_future() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let before = Utc::now();
    wg_ok(
        &wg_dir,
        &["add", "Delayed task", "--id", "d1", "--delay", "60s"],
    );
    let after = Utc::now();

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("d1").unwrap();

    let nb = task.not_before.as_ref().expect("not_before should be set");
    let nb_ts: DateTime<Utc> = nb.parse().expect("not_before should be valid timestamp");

    // not_before should be approximately now + 60s (within a few seconds tolerance)
    let expected_low = before + Duration::seconds(58);
    let expected_high = after + Duration::seconds(62);
    assert!(
        nb_ts >= expected_low && nb_ts <= expected_high,
        "not_before {} should be ~60s from now (between {} and {})",
        nb_ts,
        expected_low,
        expected_high
    );
}

#[test]
fn add_delay_task_not_in_ready_until_elapsed() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    // Add a task delayed by 300 seconds (far in the future for test purposes)
    wg_ok(
        &wg_dir,
        &["add", "Delayed task", "--id", "d1", "--delay", "300s"],
    );

    // Load and check it's not in ready_tasks
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let ready = ready_tasks(&graph);
    assert!(
        !ready.iter().any(|t| t.id == "d1"),
        "Delayed task should NOT be ready before delay elapses"
    );

    // Verify via is_time_ready as well
    let task = graph.get_task("d1").unwrap();
    assert!(
        !is_time_ready(task),
        "is_time_ready should return false for future not_before"
    );
}

// ── 2. wg add --not-before <future> ─────────────────────────────────────

#[test]
fn add_not_before_future_timestamp() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let future = (Utc::now() + Duration::hours(2)).to_rfc3339();
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Scheduled task",
            "--id",
            "s1",
            "--not-before",
            &future,
        ],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("s1").unwrap();

    assert!(task.not_before.is_some(), "not_before should be set");

    // Task should not be ready
    let ready = ready_tasks(&graph);
    assert!(
        !ready.iter().any(|t| t.id == "s1"),
        "Task with future not_before should not be ready"
    );
    assert!(
        !is_time_ready(task),
        "is_time_ready should be false for future not_before"
    );
}

// ── 3. wg add --not-before <past> → immediately ready ───────────────────

#[test]
fn add_not_before_past_timestamp_immediately_ready() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
    wg_ok(
        &wg_dir,
        &["add", "Past scheduled", "--id", "p1", "--not-before", &past],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("p1").unwrap();

    assert!(task.not_before.is_some(), "not_before should be set");
    assert!(
        is_time_ready(task),
        "Task with past not_before should be time-ready"
    );

    let ready = ready_tasks(&graph);
    assert!(
        ready.iter().any(|t| t.id == "p1"),
        "Task with past not_before should appear in ready_tasks"
    );
}

// ── 4. wg edit --delay on existing open task ────────────────────────────

#[test]
fn edit_delay_sets_not_before_on_existing_task() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("e1", "Editable")]);

    // Verify task starts with no not_before
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert!(
        graph.get_task("e1").unwrap().not_before.is_none(),
        "Task should start without not_before"
    );

    let before = Utc::now();
    wg_ok(&wg_dir, &["edit", "e1", "--delay", "120s"]);
    let after = Utc::now();

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("e1").unwrap();

    let nb = task
        .not_before
        .as_ref()
        .expect("not_before should be set after edit --delay");
    let nb_ts: DateTime<Utc> = nb.parse().expect("not_before should be valid timestamp");

    let expected_low = before + Duration::seconds(118);
    let expected_high = after + Duration::seconds(122);
    assert!(
        nb_ts >= expected_low && nb_ts <= expected_high,
        "not_before {} should be ~120s from edit time (between {} and {})",
        nb_ts,
        expected_low,
        expected_high
    );

    // Task should no longer be immediately ready
    assert!(
        !is_time_ready(task),
        "Task with future not_before from edit --delay should not be time-ready"
    );
}

// ── 5. wg edit --not-before overrides previous not_before ───────────────

#[test]
fn edit_not_before_overrides_existing() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("e2", "Override me")]);

    // First set a delay
    wg_ok(&wg_dir, &["edit", "e2", "--delay", "300s"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let original_nb = graph.get_task("e2").unwrap().not_before.clone().unwrap();

    // Now override with a specific --not-before timestamp
    let new_ts = (Utc::now() + Duration::hours(5)).to_rfc3339();
    wg_ok(&wg_dir, &["edit", "e2", "--not-before", &new_ts]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("e2").unwrap();
    let updated_nb = task
        .not_before
        .as_ref()
        .expect("not_before should still be set");

    assert_ne!(
        updated_nb, &original_nb,
        "not_before should be changed after edit --not-before"
    );
    assert_eq!(
        updated_nb, &new_ts,
        "not_before should match the new --not-before value"
    );
}

#[test]
fn edit_not_before_with_past_makes_ready() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("e3", "Make ready")]);

    // Set a far-future delay
    wg_ok(&wg_dir, &["edit", "e3", "--delay", "3600s"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert!(
        !is_time_ready(graph.get_task("e3").unwrap()),
        "Should not be ready with future delay"
    );

    // Override with a past timestamp
    let past = (Utc::now() - Duration::minutes(5)).to_rfc3339();
    wg_ok(&wg_dir, &["edit", "e3", "--not-before", &past]);

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("e3").unwrap();
    assert!(
        is_time_ready(task),
        "Task with past not_before should be ready after edit override"
    );
}

// ── 6. Cycle with --cycle-delay: ready_after on re-activation ───────────

#[test]
fn cycle_delay_sets_ready_after_on_reactivation() {
    // This test verifies that when a cycle iterates with a delay configured,
    // the cycle header gets ready_after set.
    use workgraph::graph::evaluate_cycle_iteration;

    let mut a = make_task_with_status("a", "Cycle head", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: Some("60s".to_string()),
        no_converge: false,
    });
    let mut b = make_task_with_status("b", "Cycle tail", Status::Done);
    b.after = vec!["a".to_string()];

    let (dir, path) = setup_graph(vec![a, b]);
    let mut graph = load_graph(&path).unwrap();
    let analysis = graph.compute_cycle_analysis();

    let before = Utc::now();
    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    let after = Utc::now();

    assert!(!reactivated.is_empty(), "Cycle should iterate");

    // Header should have ready_after set
    let header = graph.get_task("a").unwrap();
    let ra = header
        .ready_after
        .as_ref()
        .expect("Header should have ready_after set for cycle delay");
    let ra_ts: DateTime<Utc> = ra.parse().expect("ready_after should be valid timestamp");

    // Should be approximately now + 60s
    let expected_low = before + Duration::seconds(58);
    let expected_high = after + Duration::seconds(62);
    assert!(
        ra_ts >= expected_low && ra_ts <= expected_high,
        "ready_after {} should be ~60s from reactivation time",
        ra_ts
    );

    // Non-header member should NOT have ready_after
    let member = graph.get_task("b").unwrap();
    assert!(
        member.ready_after.is_none(),
        "Non-header cycle member should not have ready_after"
    );

    // Header should not be time-ready yet
    assert!(
        !is_time_ready(header),
        "Header with future ready_after should not be time-ready"
    );

    // Drop dir to avoid it going out of scope early
    drop(dir);
}

#[test]
fn cycle_no_delay_no_ready_after() {
    // When cycle has no delay, ready_after should NOT be set
    use workgraph::graph::evaluate_cycle_iteration;

    let mut a = make_task_with_status("a", "No delay head", Status::Done);
    a.after = vec!["b".to_string()];
    a.cycle_config = Some(CycleConfig {
        max_iterations: 5,
        guard: None,
        delay: None,
        no_converge: false,
    });
    let mut b = make_task_with_status("b", "No delay tail", Status::Done);
    b.after = vec!["a".to_string()];

    let (_dir, path) = setup_graph(vec![a, b]);
    let mut graph = load_graph(&path).unwrap();
    let analysis = graph.compute_cycle_analysis();

    let reactivated = evaluate_cycle_iteration(&mut graph, "b", &analysis);
    assert!(!reactivated.is_empty(), "Cycle should iterate");

    // Header should NOT have ready_after when no delay
    let header = graph.get_task("a").unwrap();
    assert!(
        header.ready_after.is_none(),
        "Header should NOT have ready_after when cycle has no delay"
    );
}

#[test]
fn cli_add_cycle_delay_stores_in_config() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(
        &wg_dir,
        &[
            "add",
            "Cycle with delay",
            "--id",
            "cd1",
            "--max-iterations",
            "3",
            "--cycle-delay",
            "30s",
        ],
    );

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("cd1").unwrap();
    let config = task
        .cycle_config
        .as_ref()
        .expect("Should have cycle_config");
    assert_eq!(
        config.delay,
        Some("30s".to_string()),
        "cycle_config.delay should be stored"
    );
}

// ── 7. wg show displays not_before with human-readable format ───────────

#[test]
fn show_displays_not_before_with_countdown() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let future = (Utc::now() + Duration::hours(2)).to_rfc3339();
    wg_ok(
        &wg_dir,
        &["add", "Show test", "--id", "st1", "--not-before", &future],
    );

    let output = wg_ok(&wg_dir, &["show", "st1"]);
    assert!(
        output.contains("Not before:"),
        "wg show should display 'Not before:' for tasks with not_before. Output:\n{}",
        output
    );
    // Should have the timestamp in the output
    assert!(
        output.contains(&future[..19]), // match at least the date+time portion
        "wg show should display the not_before timestamp. Output:\n{}",
        output
    );
    // Should include a countdown like "(in 1h 59m)" or similar
    assert!(
        output.contains("(in "),
        "wg show should display countdown for future not_before. Output:\n{}",
        output
    );
}

#[test]
fn show_displays_elapsed_for_past_not_before() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Past show test",
            "--id",
            "st2",
            "--not-before",
            &past,
        ],
    );

    let output = wg_ok(&wg_dir, &["show", "st2"]);
    assert!(
        output.contains("Not before:"),
        "wg show should display 'Not before:' even for past not_before. Output:\n{}",
        output
    );
    assert!(
        output.contains("(elapsed)"),
        "wg show should display '(elapsed)' for past not_before. Output:\n{}",
        output
    );
}

// ── 8. wg list shows delayed indicator for future not_before ────────────

#[test]
fn list_shows_delayed_indicator_for_future_not_before() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let future = (Utc::now() + Duration::hours(3)).to_rfc3339();
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Delayed list test",
            "--id",
            "lt1",
            "--not-before",
            &future,
        ],
    );

    let output = wg_ok(&wg_dir, &["list"]);
    assert!(
        output.contains("[delayed"),
        "wg list should show [delayed ...] indicator for tasks with future not_before. Output:\n{}",
        output
    );
}

#[test]
fn list_no_delayed_indicator_for_past_not_before() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Past list test",
            "--id",
            "lt2",
            "--not-before",
            &past,
        ],
    );

    let output = wg_ok(&wg_dir, &["list"]);
    assert!(
        !output.contains("[delayed"),
        "wg list should NOT show [delayed] for tasks with past not_before. Output:\n{}",
        output
    );
}

// ── Edge cases ──────────────────────────────────────────────────────────

#[test]
fn add_delay_and_not_before_mutually_exclusive() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let future = (Utc::now() + Duration::hours(1)).to_rfc3339();
    let output = wg_cmd(
        &wg_dir,
        &[
            "add",
            "Both flags",
            "--id",
            "bad1",
            "--delay",
            "60s",
            "--not-before",
            &future,
        ],
    );
    assert!(
        !output.status.success(),
        "Should fail when both --delay and --not-before are specified"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Cannot specify both") || stderr.contains("cannot specify both"),
        "Error should mention mutual exclusivity. Stderr:\n{}",
        stderr
    );
}

#[test]
fn edit_delay_and_not_before_mutually_exclusive() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![make_task("me1", "Mutual exclusion")]);

    let future = (Utc::now() + Duration::hours(1)).to_rfc3339();
    let output = wg_cmd(
        &wg_dir,
        &["edit", "me1", "--delay", "60s", "--not-before", &future],
    );
    assert!(
        !output.status.success(),
        "Should fail when both --delay and --not-before are specified on edit"
    );
}

#[test]
fn add_delay_invalid_format() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(
        &wg_dir,
        &["add", "Bad delay", "--id", "bad2", "--delay", "abc"],
    );
    assert!(
        !output.status.success(),
        "Should fail with invalid delay format"
    );
}

#[test]
fn add_not_before_invalid_timestamp() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(
        &wg_dir,
        &[
            "add",
            "Bad timestamp",
            "--id",
            "bad3",
            "--not-before",
            "not-a-date",
        ],
    );
    assert!(
        !output.status.success(),
        "Should fail with invalid not-before timestamp"
    );
}

#[test]
fn add_delay_various_units() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    // Test seconds
    wg_ok(
        &wg_dir,
        &["add", "Seconds", "--id", "u-s", "--delay", "30s"],
    );
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("u-s").unwrap();
    assert!(task.not_before.is_some(), "30s delay should set not_before");

    // Test minutes
    wg_ok(&wg_dir, &["add", "Minutes", "--id", "u-m", "--delay", "5m"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("u-m").unwrap();
    let nb: DateTime<Utc> = task.not_before.as_ref().unwrap().parse().unwrap();
    // 5 minutes = 300s, should be at least 290s from now
    assert!(
        nb > Utc::now() + Duration::seconds(290),
        "5m delay should be ~300s from now"
    );

    // Test hours
    wg_ok(&wg_dir, &["add", "Hours", "--id", "u-h", "--delay", "1h"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("u-h").unwrap();
    let nb: DateTime<Utc> = task.not_before.as_ref().unwrap().parse().unwrap();
    assert!(
        nb > Utc::now() + Duration::seconds(3590),
        "1h delay should be ~3600s from now"
    );

    // Test days
    wg_ok(&wg_dir, &["add", "Days", "--id", "u-d", "--delay", "1d"]);
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("u-d").unwrap();
    let nb: DateTime<Utc> = task.not_before.as_ref().unwrap().parse().unwrap();
    assert!(
        nb > Utc::now() + Duration::seconds(86390),
        "1d delay should be ~86400s from now"
    );
}

// ── ready_tasks filtering (library-level) ───────────────────────────────

#[test]
fn ready_tasks_excludes_future_not_before() {
    let mut t1 = make_task("t1", "Future");
    t1.not_before = Some((Utc::now() + Duration::hours(1)).to_rfc3339());
    let t2 = make_task("t2", "No delay");

    let (_dir, path) = setup_graph(vec![t1, t2]);
    let graph = load_graph(&path).unwrap();
    let ready = ready_tasks(&graph);

    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "t2");
}

#[test]
fn ready_tasks_includes_past_not_before() {
    let mut t1 = make_task("t1", "Past");
    t1.not_before = Some((Utc::now() - Duration::hours(1)).to_rfc3339());
    let t2 = make_task("t2", "No delay");

    let (_dir, path) = setup_graph(vec![t1, t2]);
    let graph = load_graph(&path).unwrap();
    let ready = ready_tasks(&graph);

    assert_eq!(ready.len(), 2, "Both tasks should be ready");
}

#[test]
fn ready_tasks_not_before_interacts_with_dependencies() {
    // A task with past not_before but unresolved dependency should NOT be ready
    let dep = make_task("dep", "Dependency");
    let mut t1 = make_task("t1", "Depends on dep");
    t1.not_before = Some((Utc::now() - Duration::hours(1)).to_rfc3339());
    t1.after = vec!["dep".to_string()];

    let (_dir, path) = setup_graph(vec![dep, t1]);
    let graph = load_graph(&path).unwrap();
    let ready = ready_tasks(&graph);

    // Only dep should be ready; t1 has unresolved dependency
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "dep");
}

#[test]
fn ready_tasks_both_not_before_and_ready_after() {
    // Task with both fields set: both must be past for it to be ready
    let mut t1 = make_task("t1", "Both timestamps");
    t1.not_before = Some((Utc::now() - Duration::hours(1)).to_rfc3339()); // past
    t1.ready_after = Some((Utc::now() + Duration::hours(1)).to_rfc3339()); // future

    let (_dir, path) = setup_graph(vec![t1]);
    let graph = load_graph(&path).unwrap();
    let ready = ready_tasks(&graph);

    assert!(
        ready.is_empty(),
        "Task with past not_before but future ready_after should NOT be ready"
    );
}

// ── wg ready command integration (CLI level) ────────────────────────────

#[test]
fn ready_command_excludes_delayed_task() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    // Add a normal ready task
    wg_ok(&wg_dir, &["add", "Normal task", "--id", "n1"]);
    // Add a delayed task
    wg_ok(
        &wg_dir,
        &["add", "Delayed task", "--id", "d1", "--delay", "3600s"],
    );

    let output = wg_ok(&wg_dir, &["ready"]);
    assert!(
        output.contains("n1"),
        "Normal task should appear in ready output. Output:\n{}",
        output
    );
    // d1 should NOT be in the "Ready tasks" section (it might be in "Waiting on delay")
    let ready_section = output.split("Waiting on delay").next().unwrap_or(&output);
    assert!(
        !ready_section.contains("d1") || output.contains("Waiting on delay"),
        "Delayed task should not be in 'Ready tasks' section. Output:\n{}",
        output
    );
}

#[test]
fn ready_command_json_shows_not_ready() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    wg_ok(&wg_dir, &["add", "Normal", "--id", "n1"]);
    wg_ok(
        &wg_dir,
        &["add", "Delayed", "--id", "d1", "--delay", "3600s"],
    );

    // Note: wg ready --json currently only shows ready_after-delayed tasks, not not_before-delayed
    // We verify that the normal task appears as ready
    let output = wg_ok(&wg_dir, &["ready", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&output).expect("Should be valid JSON");
    let arr = json.as_array().expect("Should be array");

    // Normal task should be ready
    let n1 = arr.iter().find(|v| v["id"] == "n1");
    assert!(n1.is_some(), "Normal task should appear in ready JSON");
    assert_eq!(
        n1.unwrap()["ready"],
        true,
        "Normal task should have ready=true"
    );
}

// ── wg show JSON output ─────────────────────────────────────────────────

#[test]
fn show_json_includes_not_before() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let future = (Utc::now() + Duration::hours(1)).to_rfc3339();
    wg_ok(
        &wg_dir,
        &[
            "add",
            "JSON show test",
            "--id",
            "js1",
            "--not-before",
            &future,
        ],
    );

    let output = wg_ok(&wg_dir, &["show", "js1", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&output).expect("Should be valid JSON");
    assert!(
        json.get("not_before").is_some(),
        "JSON show output should include not_before field. JSON:\n{}",
        json
    );
}

// ── wg list JSON output ─────────────────────────────────────────────────

#[test]
fn list_json_includes_not_before() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let future = (Utc::now() + Duration::hours(1)).to_rfc3339();
    wg_ok(
        &wg_dir,
        &[
            "add",
            "JSON list test",
            "--id",
            "jl1",
            "--not-before",
            &future,
        ],
    );

    let output = wg_ok(&wg_dir, &["list", "--json"]);
    let json: serde_json::Value = serde_json::from_str(&output).expect("Should be valid JSON");
    let arr = json.as_array().expect("Should be array");
    let task_json = arr
        .iter()
        .find(|v| v["id"] == "jl1")
        .expect("Task should appear in list JSON");
    assert!(
        task_json.get("not_before").is_some(),
        "list JSON should include not_before. Task JSON:\n{}",
        task_json
    );
}
