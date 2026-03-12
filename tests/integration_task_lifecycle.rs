//! Integration tests for task lifecycle fixes:
//! - Cascade abandon of system tasks (.evaluate-*, .verify-*)
//! - Retry + re-eval flow
//! - Supersession tracking
//! - No zombie eval/verify tasks left behind

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
    std::fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &graph_path).unwrap();
    wg_dir
}

fn graph(wg_dir: &Path) -> WorkGraph {
    load_graph(wg_dir.join("graph.jsonl")).unwrap()
}

// ===========================================================================
// Scenario 1: Cascade abandon of system tasks
// ===========================================================================

/// When a parent task is abandoned, its .evaluate-* and .verify-* children
/// should be auto-abandoned. Non-system dependents should NOT be affected.
#[test]
fn test_abandon_cascades_to_evaluate_and_verify_children() {
    let tmp = TempDir::new().unwrap();
    let mut parent = make_task("feature-x", "Implement feature X", Status::InProgress);
    parent.assigned = Some("agent-1".to_string());

    let mut eval = make_task(".evaluate-feature-x", "Eval feature X", Status::Open);
    eval.after = vec!["feature-x".to_string()];

    let mut verify = make_task(".verify-feature-x", "Verify feature X", Status::Open);
    verify.after = vec!["feature-x".to_string()];

    // A normal dependent should NOT be cascade-abandoned
    let mut downstream = make_task("deploy-x", "Deploy feature X", Status::Open);
    downstream.after = vec!["feature-x".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![parent, eval, verify, downstream]);

    wg_ok(
        &wg_dir,
        &["abandon", "feature-x", "--reason", "no longer needed"],
    );

    let g = graph(&wg_dir);
    assert_eq!(g.get_task("feature-x").unwrap().status, Status::Abandoned);
    assert_eq!(
        g.get_task(".evaluate-feature-x").unwrap().status,
        Status::Abandoned,
        ".evaluate-* should be cascade-abandoned"
    );
    assert_eq!(
        g.get_task(".verify-feature-x").unwrap().status,
        Status::Abandoned,
        ".verify-* should be cascade-abandoned"
    );
    assert_eq!(
        g.get_task("deploy-x").unwrap().status,
        Status::Open,
        "Normal dependent should NOT be cascade-abandoned"
    );
}

/// Already-terminal system tasks (Done) should not be re-abandoned.
#[test]
fn test_abandon_does_not_touch_terminal_system_tasks() {
    let tmp = TempDir::new().unwrap();
    let parent = make_task("t1", "Task 1", Status::InProgress);

    let mut eval = make_task(".evaluate-t1", "Eval t1", Status::Done);
    eval.after = vec!["t1".to_string()];

    let mut verify = make_task(".verify-t1", "Verify t1", Status::Abandoned);
    verify.after = vec!["t1".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![parent, eval, verify]);

    wg_ok(&wg_dir, &["abandon", "t1"]);

    let g = graph(&wg_dir);
    assert_eq!(g.get_task("t1").unwrap().status, Status::Abandoned);
    assert_eq!(
        g.get_task(".evaluate-t1").unwrap().status,
        Status::Done,
        "Done system task should stay Done"
    );
    assert_eq!(
        g.get_task(".verify-t1").unwrap().status,
        Status::Abandoned,
        "Already-abandoned system task should stay Abandoned"
    );
}

/// Cascade abandon should handle in-progress system tasks too.
#[test]
fn test_abandon_cascades_to_in_progress_system_tasks() {
    let tmp = TempDir::new().unwrap();
    let parent = make_task("t1", "Task 1", Status::InProgress);

    let mut eval = make_task(".evaluate-t1", "Eval t1", Status::InProgress);
    eval.after = vec!["t1".to_string()];
    eval.assigned = Some("eval-agent".to_string());

    let wg_dir = setup_workgraph(&tmp, vec![parent, eval]);

    wg_ok(&wg_dir, &["abandon", "t1"]);

    let g = graph(&wg_dir);
    assert_eq!(
        g.get_task(".evaluate-t1").unwrap().status,
        Status::Abandoned,
        "In-progress system task should be cascade-abandoned"
    );
    // Verify it has a log entry explaining why
    let eval_task = g.get_task(".evaluate-t1").unwrap();
    assert!(
        eval_task
            .log
            .iter()
            .any(|l| l.message.contains("Auto-abandoned")),
        "System task should have auto-abandon log entry"
    );
}

// ===========================================================================
// Scenario 2: Retry + re-eval flow
// ===========================================================================

/// Full lifecycle: task done → eval scheduled → task fails → retry → done again.
/// The old eval task from the first completion should not block the retry.
#[test]
fn test_retry_after_failure_with_eval_task() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);

    // Create and complete a task
    wg_ok(
        &wg_dir,
        &["add", "Flaky feature", "--id", "flaky", "--immediate"],
    );
    wg_ok(&wg_dir, &["claim", "flaky"]);

    // Simulate an eval task being created (as coordinator would)
    {
        let mut g = graph(&wg_dir);
        let eval = Task {
            id: ".evaluate-flaky".to_string(),
            title: "Eval flaky".to_string(),
            status: Status::Open,
            after: vec!["flaky".to_string()],
            ..Task::default()
        };
        g.add_node(Node::Task(eval));
        save_graph(&g, &wg_dir.join("graph.jsonl")).unwrap();
    }

    // Fail the task
    wg_ok(&wg_dir, &["fail", "flaky", "--reason", "tests broke"]);

    let g = graph(&wg_dir);
    assert_eq!(g.get_task("flaky").unwrap().status, Status::Failed);
    // Eval task should still be open (it's not cascade-abandoned on fail,
    // only on explicit abandon)

    // Retry
    wg_ok(&wg_dir, &["retry", "flaky"]);

    let g = graph(&wg_dir);
    let task = g.get_task("flaky").unwrap();
    assert_eq!(task.status, Status::Open);
    assert_eq!(task.retry_count, 1);
    assert_eq!(task.assigned, None, "Retry should clear assignment");

    // Claim and complete again
    wg_ok(&wg_dir, &["claim", "flaky"]);
    wg_ok(&wg_dir, &["done", "flaky"]);

    let g = graph(&wg_dir);
    assert_eq!(g.get_task("flaky").unwrap().status, Status::Done);
}

/// Retry clears assigned and failure_reason, allowing coordinator re-dispatch.
#[test]
fn test_retry_resets_state_for_redispatch() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("t1", "Task", Status::Failed);
    task.assigned = Some("agent-old".to_string());
    task.failure_reason = Some("OOM".to_string());
    task.retry_count = 2;
    task.tags.push("converged".to_string());

    let wg_dir = setup_workgraph(&tmp, vec![task]);

    wg_ok(&wg_dir, &["retry", "t1"]);

    let g = graph(&wg_dir);
    let t = g.get_task("t1").unwrap();
    assert_eq!(t.status, Status::Open);
    assert_eq!(t.assigned, None, "assigned should be cleared");
    assert_eq!(t.failure_reason, None, "failure_reason should be cleared");
    assert_eq!(t.retry_count, 2, "retry_count should be preserved");
    assert!(
        !t.tags.contains(&"converged".to_string()),
        "converged tag should be cleared"
    );
}

// ===========================================================================
// Scenario 3: Supersession tracking
// ===========================================================================

/// Abandon with --superseded-by records the replacement task IDs.
#[test]
fn test_supersession_via_abandon_flag() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);

    wg_ok(
        &wg_dir,
        &["add", "Original task", "--id", "original", "--immediate"],
    );
    wg_ok(
        &wg_dir,
        &["add", "Replacement A", "--id", "replace-a", "--immediate"],
    );
    wg_ok(
        &wg_dir,
        &["add", "Replacement B", "--id", "replace-b", "--immediate"],
    );

    wg_ok(
        &wg_dir,
        &[
            "abandon",
            "original",
            "--reason",
            "decomposed into smaller tasks",
            "--superseded-by",
            "replace-a,replace-b",
        ],
    );

    let g = graph(&wg_dir);
    let orig = g.get_task("original").unwrap();
    assert_eq!(orig.status, Status::Abandoned);
    assert_eq!(
        orig.superseded_by,
        vec!["replace-a".to_string(), "replace-b".to_string()],
        "superseded_by should record replacement IDs"
    );
    assert_eq!(
        orig.failure_reason.as_deref(),
        Some("decomposed into smaller tasks")
    );

    // wg show should display supersession info
    let show_output = wg_ok(&wg_dir, &["show", "original"]);
    assert!(
        show_output.contains("replace-a") || show_output.contains("supersed"),
        "wg show should display supersession info: {}",
        show_output
    );
}

/// Supersession with a single replacement task.
#[test]
fn test_supersession_single_replacement() {
    let tmp = TempDir::new().unwrap();
    let parent = make_task("old-impl", "Old implementation", Status::InProgress);
    let wg_dir = setup_workgraph(&tmp, vec![parent]);

    wg_ok(
        &wg_dir,
        &[
            "abandon",
            "old-impl",
            "--reason",
            "rewritten",
            "--superseded-by",
            "new-impl",
        ],
    );

    let g = graph(&wg_dir);
    let t = g.get_task("old-impl").unwrap();
    assert_eq!(t.superseded_by, vec!["new-impl".to_string()]);
}

// ===========================================================================
// Scenario 4: No zombie eval/verify tasks
// ===========================================================================

/// After abandoning a parent, there should be no open/in-progress system tasks
/// that depend on an abandoned parent.
#[test]
fn test_no_zombie_system_tasks_after_abandon() {
    let tmp = TempDir::new().unwrap();
    let parent = make_task("main-task", "Main task", Status::InProgress);

    let mut eval = make_task(".evaluate-main-task", "Eval main", Status::Open);
    eval.after = vec!["main-task".to_string()];

    let mut verify = make_task(".verify-main-task", "Verify main", Status::Open);
    verify.after = vec!["main-task".to_string()];

    let mut assign = make_task(".assign-main-task", "Assign main", Status::Done);
    assign.after = vec!["main-task".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![parent, eval, verify, assign]);

    wg_ok(&wg_dir, &["abandon", "main-task"]);

    let g = graph(&wg_dir);

    // Scan all system tasks that depend on the abandoned parent
    let zombies: Vec<&str> = g
        .tasks()
        .filter(|t| {
            t.id.starts_with('.')
                && t.after.contains(&"main-task".to_string())
                && !t.status.is_terminal()
        })
        .map(|t| t.id.as_str())
        .collect();

    assert!(
        zombies.is_empty(),
        "No zombie system tasks should remain after abandon, found: {:?}",
        zombies
    );
}

/// Multiple tasks abandoned in sequence — no zombie accumulation.
#[test]
fn test_no_zombie_accumulation_across_multiple_abandons() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);

    // Create 3 tasks, each with system children
    for i in 1..=3 {
        let id = format!("task-{}", i);
        wg_ok(
            &wg_dir,
            &["add", &format!("Task {}", i), "--id", &id, "--immediate"],
        );

        // Simulate eval + verify system tasks
        let mut g = graph(&wg_dir);
        let eval = Task {
            id: format!(".evaluate-{}", id),
            title: format!("Eval {}", id),
            status: Status::Open,
            after: vec![id.clone()],
            ..Task::default()
        };
        let verify = Task {
            id: format!(".verify-{}", id),
            title: format!("Verify {}", id),
            status: Status::Open,
            after: vec![id.clone()],
            ..Task::default()
        };
        g.add_node(Node::Task(eval));
        g.add_node(Node::Task(verify));
        save_graph(&g, &wg_dir.join("graph.jsonl")).unwrap();
    }

    // Abandon all 3
    for i in 1..=3 {
        wg_ok(&wg_dir, &["abandon", &format!("task-{}", i)]);
    }

    let g = graph(&wg_dir);

    // Count all non-terminal system tasks
    let zombies: Vec<String> = g
        .tasks()
        .filter(|t| t.id.starts_with('.') && !t.status.is_terminal())
        .map(|t| t.id.clone())
        .collect();

    assert!(
        zombies.is_empty(),
        "No zombie system tasks after abandoning all parents, found: {:?}",
        zombies
    );
}

// ===========================================================================
// Scenario 5: Combined lifecycle — abandon + supersede + cascade
// ===========================================================================

/// Full workflow: create task with eval/verify → abandon with supersession →
/// verify cascade + supersession are recorded correctly.
#[test]
fn test_full_lifecycle_abandon_supersede_cascade() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = tmp.path().join(".workgraph");
    wg_ok(&wg_dir, &["init"]);

    // Create original task
    wg_ok(
        &wg_dir,
        &["add", "Build auth system", "--id", "auth-v1", "--immediate"],
    );

    // Create replacement tasks
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Auth: login flow",
            "--id",
            "auth-login",
            "--immediate",
        ],
    );
    wg_ok(
        &wg_dir,
        &[
            "add",
            "Auth: token refresh",
            "--id",
            "auth-token",
            "--immediate",
        ],
    );

    // Simulate system tasks for original
    {
        let mut g = graph(&wg_dir);
        let eval = Task {
            id: ".evaluate-auth-v1".to_string(),
            title: "Eval auth-v1".to_string(),
            status: Status::Open,
            after: vec!["auth-v1".to_string()],
            ..Task::default()
        };
        let verify = Task {
            id: ".verify-auth-v1".to_string(),
            title: "Verify auth-v1".to_string(),
            status: Status::InProgress,
            after: vec!["auth-v1".to_string()],
            assigned: Some("verify-agent".to_string()),
            ..Task::default()
        };
        g.add_node(Node::Task(eval));
        g.add_node(Node::Task(verify));
        save_graph(&g, &wg_dir.join("graph.jsonl")).unwrap();
    }

    // Abandon with supersession
    wg_ok(
        &wg_dir,
        &[
            "abandon",
            "auth-v1",
            "--reason",
            "split into smaller tasks",
            "--superseded-by",
            "auth-login,auth-token",
        ],
    );

    let g = graph(&wg_dir);

    // 1. Parent abandoned
    let parent = g.get_task("auth-v1").unwrap();
    assert_eq!(parent.status, Status::Abandoned);
    assert_eq!(
        parent.superseded_by,
        vec!["auth-login".to_string(), "auth-token".to_string()]
    );

    // 2. System tasks cascade-abandoned
    assert_eq!(
        g.get_task(".evaluate-auth-v1").unwrap().status,
        Status::Abandoned
    );
    assert_eq!(
        g.get_task(".verify-auth-v1").unwrap().status,
        Status::Abandoned
    );

    // 3. Replacement tasks unaffected
    assert_eq!(g.get_task("auth-login").unwrap().status, Status::Open);
    assert_eq!(g.get_task("auth-token").unwrap().status, Status::Open);

    // 4. No zombies
    let zombies: Vec<String> = g
        .tasks()
        .filter(|t| {
            t.id.starts_with('.')
                && t.after.contains(&"auth-v1".to_string())
                && !t.status.is_terminal()
        })
        .map(|t| t.id.clone())
        .collect();
    assert!(zombies.is_empty(), "No zombie system tasks: {:?}", zombies);
}

/// Retry flow after abandon+supersede: the abandoned task cannot be retried.
#[test]
fn test_abandoned_task_cannot_be_retried() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("old", "Old task", Status::Abandoned);
    task.failure_reason = Some("superseded".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![task]);

    let output = wg_cmd(&wg_dir, &["retry", "old"]);
    assert!(
        !output.status.success(),
        "Retrying an abandoned task should fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("not failed"),
        "Error should say task is not failed, got: {}",
        combined
    );
}
