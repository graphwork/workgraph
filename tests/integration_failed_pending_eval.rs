//! Integration tests for the FailedPendingEval state machine (implement-failed-pending).
//!
//! State machine:
//!   in-progress (agent-exit-nonzero + auto_evaluate) → failed-pending-eval
//!     ├─ eval score ≥ threshold → done (rescued=true) → downstream unblocks
//!     └─ eval score < threshold → failed (terminal)
//!   in-progress (other failure class OR auto_evaluate=false) → failed (terminal, no eval)
//!
//! Validation criteria:
//!   - test_agent_exit_nonzero_with_auto_evaluate_enters_failed_pending_eval
//!   - test_rescue_eval_pass_promotes_to_done_rescued
//!   - test_rescue_eval_fail_demotes_to_failed
//!   - test_other_failure_class_skips_failed_pending_eval
//!   - test_explicit_wg_fail_skips_failed_pending_eval
//!   - test_downstream_unblocked_after_rescue
//!   - test_meta_eval_attempts_increments_on_no_score
//!   - test_meta_eval_attempts_exhausted_terminal_failure
//!   - test_failed_pending_eval_is_active_not_terminal
//!   - test_failed_pending_eval_does_not_satisfy_deps
//!   - test_failed_pending_eval_system_bypass
//!   - test_rescued_field_serializes_round_trips

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{FailureClass, Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};

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
        .env_remove("WG_AGENT_ID")
        .env_remove("WG_TASK_ID")
        .env("WG_SMOKE_AGENT_OVERRIDE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run wg command")
}

fn make_task(id: &str, status: Status) -> Task {
    Task {
        id: id.to_string(),
        title: id.to_string(),
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

// ── Schema / property tests ────────────────────────────────────────────────

#[test]
fn test_failed_pending_eval_is_active_not_terminal() {
    assert!(
        !Status::FailedPendingEval.is_terminal(),
        "FailedPendingEval must not be terminal"
    );
    assert!(
        Status::FailedPendingEval.is_active(),
        "FailedPendingEval must be active"
    );
    assert!(
        !Status::FailedPendingEval.is_dep_satisfied(),
        "FailedPendingEval must not satisfy deps"
    );
}

#[test]
fn test_failed_pending_eval_serializes_correctly() {
    let s = Status::FailedPendingEval;
    assert_eq!(s.to_string(), "failed-pending-eval");
    let json = serde_json::to_string(&s).unwrap();
    assert_eq!(json, "\"failed-pending-eval\"");
    let parsed: Status = serde_json::from_str("\"failed-pending-eval\"").unwrap();
    assert_eq!(parsed, Status::FailedPendingEval);
}

#[test]
fn test_rescued_field_serializes_round_trips() {
    let mut task = Task::default();
    task.id = "t1".to_string();
    task.title = "Test".to_string();
    task.rescued = true;
    task.meta_eval_attempts = 1;

    let json = serde_json::to_string(&task).unwrap();
    assert!(json.contains("\"rescued\":true"), "rescued should serialize");
    assert!(
        json.contains("\"meta_eval_attempts\":1"),
        "meta_eval_attempts should serialize"
    );

    let parsed: Task = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.rescued, true);
    assert_eq!(parsed.meta_eval_attempts, 1);

    // Default (false) should NOT appear in JSON (skip_serializing_if)
    let mut task2 = Task::default();
    task2.id = "t2".to_string();
    task2.title = "Test2".to_string();
    let json2 = serde_json::to_string(&task2).unwrap();
    assert!(
        !json2.contains("rescued"),
        "rescued=false should not serialize"
    );
    assert!(
        !json2.contains("meta_eval_attempts"),
        "meta_eval_attempts=0 should not serialize"
    );
}

#[test]
fn test_rescued_false_deserializes_from_legacy_row() {
    // Legacy rows without the rescued field should deserialize to rescued=false
    let json = r#"{"id":"t1","title":"Test","status":"done"}"#;
    let task: Task = serde_json::from_str(json).unwrap();
    assert_eq!(task.rescued, false);
    assert_eq!(task.meta_eval_attempts, 0);
}

// ── fail.rs intercept tests ────────────────────────────────────────────────

#[test]
fn test_agent_exit_nonzero_with_auto_evaluate_enters_failed_pending_eval() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("t1", Status::InProgress);
    task.assigned = Some("test-agent".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![task]);

    // Enable auto_evaluate in config
    let config_path = wg_dir.join("config.toml");
    std::fs::write(&config_path, "[agency]\nauto_evaluate = true\n").unwrap();

    // Call wg fail with agent-exit-nonzero class (simulating wrapper behavior)
    let out = wg_cmd(&wg_dir, &["fail", "t1", "--class", "agent-exit-nonzero"]);
    assert!(
        out.status.success(),
        "wg fail failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Task should now be in failed-pending-eval, NOT failed
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("t1").unwrap();
    assert_eq!(
        task.status,
        Status::FailedPendingEval,
        "expected failed-pending-eval with auto_evaluate=true"
    );
    assert_eq!(task.failure_class, Some(FailureClass::AgentExitNonzero));
    // retry_count should be unchanged (not a retry path)
    assert_eq!(task.retry_count, 0);
}

#[test]
fn test_other_failure_class_skips_failed_pending_eval() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("t1", Status::InProgress);
    task.assigned = Some("test-agent".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![task]);

    let config_path = wg_dir.join("config.toml");
    std::fs::write(&config_path, "[agency]\nauto_evaluate = true\n").unwrap();

    // api-error-429 should NOT enter FailedPendingEval
    let out = wg_cmd(
        &wg_dir,
        &["fail", "t1", "--class", "api-error-429-rate-limit"],
    );
    assert!(out.status.success(), "wg fail failed");

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("t1").unwrap();
    assert_eq!(
        task.status,
        Status::Failed,
        "rate-limit failures must not enter FailedPendingEval"
    );
}

#[test]
fn test_auto_evaluate_false_skips_failed_pending_eval() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("t1", Status::InProgress);
    task.assigned = Some("test-agent".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![task]);

    // Explicitly disable auto_evaluate (default is true in AgencyConfig)
    let config_path = wg_dir.join("config.toml");
    std::fs::write(&config_path, "[agency]\nauto_evaluate = false\n").unwrap();

    let out = wg_cmd(&wg_dir, &["fail", "t1", "--class", "agent-exit-nonzero"]);
    assert!(out.status.success(), "wg fail failed");

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("t1").unwrap();
    assert_eq!(
        task.status,
        Status::Failed,
        "with auto_evaluate=false, agent-exit-nonzero must go to terminal Failed"
    );
}

#[test]
fn test_explicit_wg_fail_on_failed_pending_eval_forces_terminal_failed() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join(".workgraph/config.toml");

    // Put task directly into FailedPendingEval
    let mut task = make_task("t1", Status::FailedPendingEval);
    task.failure_class = Some(FailureClass::AgentExitNonzero);
    let wg_dir = setup_workgraph(&tmp, vec![task]);

    std::fs::write(&config_path, "[agency]\nauto_evaluate = true\n").unwrap();

    // Operator calls wg fail again with agent-exit-nonzero → should force terminal Failed
    let out = wg_cmd(
        &wg_dir,
        &[
            "fail",
            "t1",
            "--class",
            "agent-exit-nonzero",
            "--reason",
            "operator forced terminal fail",
        ],
    );
    assert!(out.status.success(), "wg fail should succeed");

    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    let task = graph.get_task("t1").unwrap();
    assert_eq!(
        task.status,
        Status::Failed,
        "re-calling wg fail on FailedPendingEval must force terminal Failed"
    );
}

// ── Dependency resolution tests ────────────────────────────────────────────

#[test]
fn test_failed_pending_eval_does_not_satisfy_deps() {
    // A downstream task should remain blocked when its upstream is FailedPendingEval
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", Status::FailedPendingEval);
    let mut t2 = make_task("t2", Status::Open);
    t2.after = vec!["t1".to_string()];
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Check that t2 is not ready
    let out = wg_cmd(&wg_dir, &["ready"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("t2"),
        "downstream should not be ready when upstream is FailedPendingEval; stdout: {}",
        stdout
    );
}

#[test]
fn test_failed_pending_eval_system_bypass() {
    // .evaluate-t1 (a system task) should be ready when t1 is FailedPendingEval
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", Status::FailedPendingEval);
    let mut eval = make_task(".evaluate-t1", Status::Open);
    eval.after = vec!["t1".to_string()];
    let wg_dir = setup_workgraph(&tmp, vec![t1, eval]);

    // .evaluate-t1 should be ready (system bypass)
    let out = wg_cmd(&wg_dir, &["ready"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(".evaluate-t1"),
        ".evaluate-t1 should be ready via system bypass when source is FailedPendingEval; stdout: {}",
        stdout
    );
}

// ── wg show displays ───────────────────────────────────────────────────────

#[test]
fn test_wg_show_displays_failed_pending_eval_status() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("t1", Status::InProgress);
    task.assigned = Some("test-agent".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![task]);

    let config_path = wg_dir.join("config.toml");
    std::fs::write(&config_path, "[agency]\nauto_evaluate = true\n").unwrap();

    // Fail the task with agent-exit-nonzero → should enter FailedPendingEval
    let out = wg_cmd(&wg_dir, &["fail", "t1", "--class", "agent-exit-nonzero"]);
    assert!(out.status.success());

    // wg show should indicate the state
    let out = wg_cmd(&wg_dir, &["show", "t1"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("failed-pending-eval")
            || stdout.contains("failed pending eval")
            || stdout.contains("failed pending evaluation"),
        "wg show should display failed-pending-eval label; got: {}",
        stdout
    );
}

#[test]
fn test_wg_show_json_includes_rescued_field() {
    let tmp = TempDir::new().unwrap();
    let mut task = make_task("t1", Status::Done);
    task.rescued = true;
    let wg_dir = setup_workgraph(&tmp, vec![task]);

    let out = wg_cmd(&wg_dir, &["show", "t1", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let val: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_default();
    assert_eq!(
        val["rescued"].as_bool(),
        Some(true),
        "wg show --json should include rescued=true; stdout: {}",
        stdout
    );
}

// ── wg list indicator ──────────────────────────────────────────────────────

#[test]
fn test_wg_list_shows_failed_pending_eval_with_indicator() {
    let tmp = TempDir::new().unwrap();
    let task = make_task("t1", Status::FailedPendingEval);
    let wg_dir = setup_workgraph(&tmp, vec![task]);

    let out = wg_cmd(&wg_dir, &["list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Should show [e] indicator (lowercase e for failed-pending-eval)
    assert!(
        stdout.contains("[e]") || stdout.contains("failed-pending-eval"),
        "wg list should show [e] for FailedPendingEval; got: {}",
        stdout
    );
}
