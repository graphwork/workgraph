//! End-to-end smoke tests for the failed-dependency triage protocol.
//!
//! These tests exercise the full triage lifecycle through the real `wg` binary
//! in isolated temp directories. They simulate what the coordinator + agents do
//! during triage without requiring LLM calls.
//!
//! Scenarios:
//! 1. Basic: A fails → B enters triage → fix created → chain recovers
//! 2. Cascading: triage fix also fails → loop guard prevents infinite triage
//! 3. Multiple failed deps: task depends on A and B, both failed
//! 4. Regression: fix succeeds, but re-run fails for new reason → re-triage

use std::path::Path;
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::Status;
use workgraph::parser::load_graph;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn wg_binary() -> std::path::PathBuf {
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

fn graph(wg_dir: &Path) -> workgraph::graph::WorkGraph {
    load_graph(wg_dir.join("graph.jsonl")).unwrap()
}

fn task_status(wg_dir: &Path, id: &str) -> Status {
    graph(wg_dir).get_task(id).unwrap().status.clone()
}

fn task_triage_count(wg_dir: &Path, id: &str) -> u32 {
    graph(wg_dir).get_task(id).unwrap().triage_count
}

fn is_ready(wg_dir: &Path, id: &str) -> bool {
    let output = wg_ok(wg_dir, &["ready"]);
    output.contains(id)
}

/// Set up a fresh workgraph and return the .workgraph dir path.
fn setup() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);
    (tmp, wg_dir)
}

// ---------------------------------------------------------------------------
// Scenario 1: Basic triage — A fails → B triages → fix → chain recovers
// ---------------------------------------------------------------------------

/// Full triage lifecycle:
///   task-a → task-b → task-c
///   task-a fails → task-b becomes ready (failed dep is terminal) →
///   agent claims task-b, enters triage → creates fix-a → adds dep →
///   retries task-a → requeues task-b → fix-a completes →
///   task-a re-runs and succeeds → task-b dispatches normally → task-c runs
#[test]
fn smoke_triage_basic_chain_recovery() {
    let (_tmp, wg_dir) = setup();

    // Build the chain: task-a → task-b → task-c
    wg_ok(
        &wg_dir,
        &["add", "Task A", "--id", "task-a", "--immediate"],
    );
    wg_ok(
        &wg_dir,
        &[
            "add", "Task B", "--id", "task-b", "--after", "task-a", "--immediate",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "add", "Task C", "--id", "task-c", "--after", "task-b", "--immediate",
        ],
    );

    // ── Step 1: task-a fails ────────────────────────────────────────────
    wg_ok(&wg_dir, &["claim", "task-a"]);
    wg_ok(
        &wg_dir,
        &["fail", "task-a", "--reason", "assertion error in parser"],
    );
    assert_eq!(task_status(&wg_dir, "task-a"), Status::Failed);

    // ── Step 2: task-b becomes ready (failed dep is terminal) ───────────
    assert!(
        is_ready(&wg_dir, "task-b"),
        "task-b should be ready when task-a is Failed (terminal)"
    );
    assert!(
        !is_ready(&wg_dir, "task-c"),
        "task-c should NOT be ready (task-b is still Open)"
    );

    // ── Step 3: Agent claims task-b (would enter triage mode) ───────────
    wg_ok(&wg_dir, &["claim", "task-b"]);
    assert_eq!(task_status(&wg_dir, "task-b"), Status::InProgress);

    // ── Step 4: Agent performs triage — create fix task ──────────────────
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Fix: parser assertion error",
            "--id",
            "fix-a",
            "--immediate",
        ],
    );
    // Wire fix-a as a dependency of task-a
    wg_ok(&wg_dir, &["add-dep", "task-a", "fix-a"]);

    // ── Step 5: Agent retries task-a ────────────────────────────────────
    wg_ok(&wg_dir, &["retry", "task-a"]);
    assert_eq!(task_status(&wg_dir, "task-a"), Status::Open);

    // ── Step 6: Agent requeues itself ───────────────────────────────────
    wg_ok(
        &wg_dir,
        &[
            "requeue",
            "task-b",
            "--reason",
            "Created fix for failed dep task-a",
        ],
    );
    assert_eq!(task_status(&wg_dir, "task-b"), Status::Open);
    assert_eq!(task_triage_count(&wg_dir, "task-b"), 1);

    // ── Step 7: Verify blocking — task-a blocked by fix-a ───────────────
    assert!(
        !is_ready(&wg_dir, "task-a"),
        "task-a should be blocked by fix-a"
    );
    assert!(
        is_ready(&wg_dir, "fix-a"),
        "fix-a should be ready (no deps)"
    );

    // ── Step 8: Fix completes → task-a becomes ready ────────────────────
    wg_ok(&wg_dir, &["claim", "fix-a"]);
    wg_ok(&wg_dir, &["done", "fix-a"]);
    assert!(
        is_ready(&wg_dir, "task-a"),
        "task-a should be ready after fix-a completes"
    );

    // ── Step 9: task-a succeeds → task-b becomes ready ──────────────────
    wg_ok(&wg_dir, &["claim", "task-a"]);
    wg_ok(&wg_dir, &["done", "task-a"]);
    assert_eq!(task_status(&wg_dir, "task-a"), Status::Done);
    assert!(
        is_ready(&wg_dir, "task-b"),
        "task-b should be ready after task-a succeeds"
    );

    // ── Step 10: task-b runs normally → task-c runs ─────────────────────
    wg_ok(&wg_dir, &["claim", "task-b"]);
    wg_ok(&wg_dir, &["done", "task-b"]);
    assert_eq!(task_status(&wg_dir, "task-b"), Status::Done);
    assert_eq!(task_triage_count(&wg_dir, "task-b"), 1); // preserved

    assert!(
        is_ready(&wg_dir, "task-c"),
        "task-c should be ready after task-b completes"
    );
    wg_ok(&wg_dir, &["claim", "task-c"]);
    wg_ok(&wg_dir, &["done", "task-c"]);
    assert_eq!(task_status(&wg_dir, "task-c"), Status::Done);

    // ── Final: no orphaned tasks ────────────────────────────────────────
    let g = graph(&wg_dir);
    for task in g.tasks() {
        assert_eq!(
            task.status,
            Status::Done,
            "All tasks should be done, but {} is {:?}",
            task.id,
            task.status
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 2: Cascading — triage fix also fails → loop guard kicks in
// ---------------------------------------------------------------------------

/// task-a fails → task-b triages → fix-a fails → task-b triages again →
/// fix-a-v2 fails → task-b triages (3rd time) → 4th requeue hits budget → fails.
#[test]
fn smoke_triage_cascading_failure_loop_guard() {
    let (_tmp, wg_dir) = setup();

    // Build: task-a → task-b
    wg_ok(
        &wg_dir,
        &["add", "Task A", "--id", "task-a", "--immediate"],
    );
    wg_ok(
        &wg_dir,
        &[
            "add", "Task B", "--id", "task-b", "--after", "task-a", "--immediate",
        ],
    );

    // ── Round 1: task-a fails, task-b triages ───────────────────────────
    wg_ok(&wg_dir, &["claim", "task-a"]);
    wg_ok(
        &wg_dir,
        &["fail", "task-a", "--reason", "OOM on large input"],
    );

    wg_ok(&wg_dir, &["claim", "task-b"]);
    // Agent creates fix-v1
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Fix: increase memory limit",
            "--id",
            "fix-v1",
            "--immediate",
        ],
    );
    wg_ok(&wg_dir, &["add-dep", "task-a", "fix-v1"]);
    wg_ok(&wg_dir, &["retry", "task-a"]);
    wg_ok(
        &wg_dir,
        &["requeue", "task-b", "--reason", "Created fix-v1 for task-a"],
    );
    assert_eq!(task_triage_count(&wg_dir, "task-b"), 1);

    // fix-v1 fails too
    wg_ok(&wg_dir, &["claim", "fix-v1"]);
    wg_ok(
        &wg_dir,
        &["fail", "fix-v1", "--reason", "memory limit config not found"],
    );

    // task-a is still blocked by fix-v1 (which is Failed=terminal),
    // but task-a is Open — it becomes ready since all deps are terminal
    // Actually task-a now has fix-v1 as dep (which is Failed) — terminal, so task-a is ready
    // task-a runs again but fails again
    wg_ok(&wg_dir, &["claim", "task-a"]);
    wg_ok(
        &wg_dir,
        &["fail", "task-a", "--reason", "OOM persists after fix-v1 failed"],
    );

    // ── Round 2: task-b triages again ───────────────────────────────────
    wg_ok(&wg_dir, &["claim", "task-b"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Fix: streaming approach",
            "--id",
            "fix-v2",
            "--immediate",
        ],
    );
    wg_ok(&wg_dir, &["add-dep", "task-a", "fix-v2"]);
    wg_ok(&wg_dir, &["retry", "task-a"]);
    wg_ok(
        &wg_dir,
        &["requeue", "task-b", "--reason", "Created fix-v2 for task-a"],
    );
    assert_eq!(task_triage_count(&wg_dir, "task-b"), 2);

    // fix-v2 fails
    wg_ok(&wg_dir, &["claim", "fix-v2"]);
    wg_ok(
        &wg_dir,
        &["fail", "fix-v2", "--reason", "streaming not supported"],
    );

    // task-a runs again and fails
    wg_ok(&wg_dir, &["claim", "task-a"]);
    wg_ok(
        &wg_dir,
        &["fail", "task-a", "--reason", "still OOM"],
    );

    // ── Round 3: task-b triages one more time ───────────────────────────
    wg_ok(&wg_dir, &["claim", "task-b"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Fix: reduce input size",
            "--id",
            "fix-v3",
            "--immediate",
        ],
    );
    wg_ok(&wg_dir, &["add-dep", "task-a", "fix-v3"]);
    wg_ok(&wg_dir, &["retry", "task-a"]);
    wg_ok(
        &wg_dir,
        &["requeue", "task-b", "--reason", "Created fix-v3 for task-a"],
    );
    assert_eq!(task_triage_count(&wg_dir, "task-b"), 3);

    // fix-v3 fails
    wg_ok(&wg_dir, &["claim", "fix-v3"]);
    wg_ok(
        &wg_dir,
        &["fail", "fix-v3", "--reason", "input reduction not feasible"],
    );

    // task-a runs and fails again
    wg_ok(&wg_dir, &["claim", "task-a"]);
    wg_ok(
        &wg_dir,
        &["fail", "task-a", "--reason", "still OOM after all fixes"],
    );

    // ── Round 4: loop guard kicks in ────────────────────────────────────
    wg_ok(&wg_dir, &["claim", "task-b"]);

    let output = wg_cmd(
        &wg_dir,
        &[
            "requeue",
            "task-b",
            "--reason",
            "attempted 4th triage round",
        ],
    );
    assert!(
        !output.status.success(),
        "4th requeue should fail (default budget is 3)"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Triage budget exhausted"),
        "Should report budget exhausted, got: {}",
        stderr
    );
    assert_eq!(
        task_triage_count(&wg_dir, "task-b"),
        3,
        "triage_count should stay at 3 after budget rejection"
    );

    // Verify the task can still be failed gracefully
    wg_ok(
        &wg_dir,
        &[
            "fail",
            "task-b",
            "--reason",
            "Triage budget exhausted, dependency task-a unfixable",
        ],
    );
    assert_eq!(task_status(&wg_dir, "task-b"), Status::Failed);
}

// ---------------------------------------------------------------------------
// Scenario 3: Multiple failed deps
// ---------------------------------------------------------------------------

/// task-m depends on task-x AND task-y, both of which fail.
/// Triage should handle both failed deps (create fixes for each).
#[test]
fn smoke_triage_multiple_failed_deps() {
    let (_tmp, wg_dir) = setup();

    // Build: task-x ──┐
    //                  ├──→ task-m
    // Build: task-y ──┘
    wg_ok(
        &wg_dir,
        &["add", "Task X", "--id", "task-x", "--immediate"],
    );
    wg_ok(
        &wg_dir,
        &["add", "Task Y", "--id", "task-y", "--immediate"],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Task M (multi-dep)",
            "--id",
            "task-m",
            "--after",
            "task-x,task-y",
            "--immediate",
        ],
    );

    // Both deps fail
    wg_ok(&wg_dir, &["claim", "task-x"]);
    wg_ok(
        &wg_dir,
        &["fail", "task-x", "--reason", "network timeout"],
    );
    wg_ok(&wg_dir, &["claim", "task-y"]);
    wg_ok(
        &wg_dir,
        &["fail", "task-y", "--reason", "validation error"],
    );

    // task-m should be ready (both deps are terminal)
    assert!(
        is_ready(&wg_dir, "task-m"),
        "task-m should be ready when all deps are terminal (Failed)"
    );

    // ── Triage: agent claims task-m, creates fixes for both deps ────────
    wg_ok(&wg_dir, &["claim", "task-m"]);

    // Fix for task-x
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Fix: network retry logic",
            "--id",
            "fix-x",
            "--immediate",
        ],
    );
    wg_ok(&wg_dir, &["add-dep", "task-x", "fix-x"]);
    wg_ok(&wg_dir, &["retry", "task-x"]);

    // Fix for task-y
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Fix: validation schema",
            "--id",
            "fix-y",
            "--immediate",
        ],
    );
    wg_ok(&wg_dir, &["add-dep", "task-y", "fix-y"]);
    wg_ok(&wg_dir, &["retry", "task-y"]);

    // Requeue task-m
    wg_ok(
        &wg_dir,
        &[
            "requeue",
            "task-m",
            "--reason",
            "Created fixes for failed deps task-x and task-y",
        ],
    );
    assert_eq!(task_triage_count(&wg_dir, "task-m"), 1);
    assert_eq!(task_status(&wg_dir, "task-m"), Status::Open);

    // ── Both fixes should be ready ──────────────────────────────────────
    assert!(is_ready(&wg_dir, "fix-x"), "fix-x should be ready");
    assert!(is_ready(&wg_dir, "fix-y"), "fix-y should be ready");

    // ── Complete fixes → deps re-run → task-m recovers ──────────────────
    wg_ok(&wg_dir, &["claim", "fix-x"]);
    wg_ok(&wg_dir, &["done", "fix-x"]);
    wg_ok(&wg_dir, &["claim", "fix-y"]);
    wg_ok(&wg_dir, &["done", "fix-y"]);

    // task-x and task-y should be ready now
    assert!(
        is_ready(&wg_dir, "task-x"),
        "task-x should be ready after fix-x completes"
    );
    assert!(
        is_ready(&wg_dir, "task-y"),
        "task-y should be ready after fix-y completes"
    );

    // Complete the original deps
    wg_ok(&wg_dir, &["claim", "task-x"]);
    wg_ok(&wg_dir, &["done", "task-x"]);
    wg_ok(&wg_dir, &["claim", "task-y"]);
    wg_ok(&wg_dir, &["done", "task-y"]);

    // task-m should be ready now
    assert!(
        is_ready(&wg_dir, "task-m"),
        "task-m should be ready after both deps succeed"
    );

    // Complete the chain
    wg_ok(&wg_dir, &["claim", "task-m"]);
    wg_ok(&wg_dir, &["done", "task-m"]);
    assert_eq!(task_status(&wg_dir, "task-m"), Status::Done);

    // ── Verify: no orphaned open tasks ──────────────────────────────────
    let g = graph(&wg_dir);
    for task in g.tasks() {
        assert!(
            task.status == Status::Done,
            "Task {} should be done, got {:?}",
            task.id,
            task.status
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 4: Regression — fix works but re-run fails for new reason
// ---------------------------------------------------------------------------

/// task-a fails (reason 1) → task-b triages → fix succeeds → task-a re-runs
/// but fails for a DIFFERENT reason → task-b triages again with new context.
#[test]
fn smoke_triage_regression_new_failure() {
    let (_tmp, wg_dir) = setup();

    // Build: task-a → task-b
    wg_ok(
        &wg_dir,
        &["add", "Task A", "--id", "task-a", "--immediate"],
    );
    wg_ok(
        &wg_dir,
        &[
            "add", "Task B", "--id", "task-b", "--after", "task-a", "--immediate",
        ],
    );

    // ── Round 1: task-a fails with reason 1 ─────────────────────────────
    wg_ok(&wg_dir, &["claim", "task-a"]);
    wg_ok(
        &wg_dir,
        &["fail", "task-a", "--reason", "missing config file"],
    );

    // task-b enters triage
    wg_ok(&wg_dir, &["claim", "task-b"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Fix: create default config",
            "--id",
            "fix-config",
            "--immediate",
        ],
    );
    wg_ok(&wg_dir, &["add-dep", "task-a", "fix-config"]);
    wg_ok(&wg_dir, &["retry", "task-a"]);
    wg_ok(
        &wg_dir,
        &[
            "requeue",
            "task-b",
            "--reason",
            "Created fix-config for task-a",
        ],
    );
    assert_eq!(task_triage_count(&wg_dir, "task-b"), 1);

    // Fix succeeds
    wg_ok(&wg_dir, &["claim", "fix-config"]);
    wg_ok(&wg_dir, &["done", "fix-config"]);

    // ── task-a re-runs but fails for a NEW reason ───────────────────────
    wg_ok(&wg_dir, &["claim", "task-a"]);
    wg_ok(
        &wg_dir,
        &[
            "fail",
            "task-a",
            "--reason",
            "config loaded but schema validation failed",
        ],
    );

    // Verify task-a has a new failure reason
    let g = graph(&wg_dir);
    let a = g.get_task("task-a").unwrap();
    assert_eq!(
        a.failure_reason.as_deref(),
        Some("config loaded but schema validation failed"),
        "task-a should have the new failure reason"
    );

    // ── Round 2: task-b triages again (different reason) ────────────────
    assert!(
        is_ready(&wg_dir, "task-b"),
        "task-b should be ready (task-a is Failed=terminal)"
    );

    wg_ok(&wg_dir, &["claim", "task-b"]);
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Fix: add schema migration",
            "--id",
            "fix-schema",
            "--immediate",
        ],
    );
    wg_ok(&wg_dir, &["add-dep", "task-a", "fix-schema"]);
    wg_ok(&wg_dir, &["retry", "task-a"]);
    wg_ok(
        &wg_dir,
        &[
            "requeue",
            "task-b",
            "--reason",
            "Created fix-schema for task-a (new failure: schema validation)",
        ],
    );
    assert_eq!(task_triage_count(&wg_dir, "task-b"), 2);

    // Fix succeeds
    wg_ok(&wg_dir, &["claim", "fix-schema"]);
    wg_ok(&wg_dir, &["done", "fix-schema"]);

    // task-a succeeds this time
    wg_ok(&wg_dir, &["claim", "task-a"]);
    wg_ok(&wg_dir, &["done", "task-a"]);
    assert_eq!(task_status(&wg_dir, "task-a"), Status::Done);

    // task-b finally runs normally
    assert!(
        is_ready(&wg_dir, "task-b"),
        "task-b should be ready after task-a succeeds"
    );
    wg_ok(&wg_dir, &["claim", "task-b"]);
    wg_ok(&wg_dir, &["done", "task-b"]);
    assert_eq!(task_status(&wg_dir, "task-b"), Status::Done);
    assert_eq!(
        task_triage_count(&wg_dir, "task-b"),
        2,
        "triage_count should reflect both triage rounds"
    );

    // ── Verify: all tasks done ──────────────────────────────────────────
    let g = graph(&wg_dir);
    for task in g.tasks() {
        assert_eq!(
            task.status,
            Status::Done,
            "Task {} should be done, got {:?}",
            task.id,
            task.status
        );
    }
}
