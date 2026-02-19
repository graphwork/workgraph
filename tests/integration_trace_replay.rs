//! Integration tests for the full trace + replay + runs workflow.
//!
//! Tests exercise these CLI commands end-to-end via the `wg` binary:
//! - `wg trace <id>` / `wg trace <id> --json`
//! - `wg replay --failed-only` / `--below-score` / `--plan-only` / `--subgraph`
//! - `wg runs list` / `wg runs show <run-id>`

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
        args,
        stdout,
        stderr
    );
    stdout
}

fn wg_json(wg_dir: &Path, args: &[&str]) -> String {
    let mut full_args = vec!["--json"];
    full_args.extend_from_slice(args);
    wg_ok(wg_dir, &full_args)
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
// 1-2. Create a task graph and verify trace returns data
// ===========================================================================

#[test]
fn test_trace_returns_structured_data() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("build", "Build project", Status::Done);
    t1.blocks = vec!["test".to_string(), "lint".to_string()];
    let mut t2 = make_task("test", "Run tests", Status::Done);
    t2.blocked_by = vec!["build".to_string()];
    t2.blocks = vec!["deploy".to_string()];
    let mut t3 = make_task("lint", "Lint code", Status::Failed);
    t3.blocked_by = vec!["build".to_string()];
    t3.failure_reason = Some("lint errors".to_string());
    let mut t4 = make_task("deploy", "Deploy app", Status::Open);
    t4.blocked_by = vec!["test".to_string()];
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2, t3, t4]);

    // Trace each task and verify output is non-empty
    for task_id in &["build", "test", "lint", "deploy"] {
        let output = wg_ok(&wg_dir, &["trace", task_id]);
        assert!(
            output.contains(&format!("Trace: {}", task_id)),
            "trace output for {} should contain header: {}",
            task_id,
            output
        );
    }
}

// ===========================================================================
// 3-4. Verify wg trace --json output is parseable
// ===========================================================================

#[test]
fn test_trace_json_is_parseable() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("alpha", "Alpha task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let output = wg_json(&wg_dir, &["trace", "alpha"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap_or_else(|e| {
        panic!(
            "trace --json output should be valid JSON.\nError: {}\nOutput: {}",
            e, output
        )
    });

    assert_eq!(parsed["id"], "alpha");
    assert_eq!(parsed["title"], "Alpha task");
    assert_eq!(parsed["status"], "done");
    assert!(parsed["operations"].is_array());
    assert!(parsed["agent_runs"].is_array());
    assert!(parsed["summary"].is_object());
    assert!(parsed["summary"]["operation_count"].is_number());
    assert!(parsed["summary"]["agent_run_count"].is_number());
}

// ===========================================================================
// 5-8. Replay --failed-only: snapshot, reset, preserve, dependents
// ===========================================================================

#[test]
fn test_replay_failed_only_resets_and_preserves() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("root", "Root task", Status::Done);
    t1.blocks = vec!["mid".to_string()];
    let mut t2 = make_task("mid", "Middle task", Status::Failed);
    t2.blocked_by = vec!["root".to_string()];
    t2.blocks = vec!["leaf".to_string()];
    t2.failure_reason = Some("compile error".to_string());
    let mut t3 = make_task("leaf", "Leaf task", Status::Done);
    t3.blocked_by = vec!["mid".to_string()];
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2, t3]);

    // Run replay --failed-only --model different-model
    wg_ok(
        &wg_dir,
        &["replay", "--failed-only", "--model", "different-model"],
    );

    // 6. Verify snapshot was created in .workgraph/runs/
    let runs_dir = wg_dir.join("runs");
    assert!(runs_dir.exists(), "runs/ directory should exist after replay");
    let run_entries: Vec<_> = fs::read_dir(&runs_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(
        run_entries.len(),
        1,
        "should have exactly 1 run snapshot after replay"
    );

    // Verify snapshot contents
    let run_dir = run_entries[0].path();
    assert!(run_dir.join("graph.jsonl").exists());
    assert!(run_dir.join("meta.json").exists());

    // 7. Verify only failed tasks were reset (done tasks preserved)
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(
        graph.get_task("root").unwrap().status,
        Status::Done,
        "root (Done) should be preserved"
    );
    assert_eq!(
        graph.get_task("mid").unwrap().status,
        Status::Open,
        "mid (Failed) should be reset to Open"
    );
    // 8. Verify transitive dependents were also reset
    assert_eq!(
        graph.get_task("leaf").unwrap().status,
        Status::Open,
        "leaf (transitive dependent of mid) should be reset"
    );

    // Verify model was applied to reset tasks
    assert_eq!(
        graph.get_task("mid").unwrap().model,
        Some("different-model".to_string())
    );
    assert_eq!(
        graph.get_task("leaf").unwrap().model,
        Some("different-model".to_string())
    );
}

// ===========================================================================
// 9-10. Runs list & show
// ===========================================================================

#[test]
fn test_runs_list_and_show() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("a", "Task A", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Initially no runs
    let output = wg_ok(&wg_dir, &["runs", "list"]);
    assert!(
        output.contains("No run snapshots") || output.contains("[]"),
        "should show no runs initially: {}",
        output
    );

    // Create a replay to generate a run
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // 9. Verify wg runs list shows the run
    let output = wg_ok(&wg_dir, &["runs", "list"]);
    assert!(
        output.contains("run-001"),
        "runs list should contain run-001: {}",
        output
    );

    // 10. Verify wg runs show <run-id> shows metadata
    let output = wg_ok(&wg_dir, &["runs", "show", "run-001"]);
    assert!(
        output.contains("run-001"),
        "runs show should contain run ID: {}",
        output
    );
    assert!(
        output.contains("Reset tasks") || output.contains("reset"),
        "runs show should contain reset info: {}",
        output
    );

    // Verify JSON output is parseable
    let json_output = wg_json(&wg_dir, &["runs", "show", "run-001"]);
    let parsed: serde_json::Value = serde_json::from_str(&json_output).unwrap();
    assert_eq!(parsed["id"], "run-001");
    assert!(parsed["reset_tasks"].is_array());
    assert!(parsed["preserved_tasks"].is_array());
}

// ===========================================================================
// 11. Provenance log entries for snapshot + reset
// ===========================================================================

#[test]
fn test_replay_creates_provenance_entries() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("p1", "Provenance test", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Read provenance log and verify replay entry exists
    let ops = workgraph::provenance::read_all_operations(&wg_dir).unwrap();
    let replay_ops: Vec<_> = ops.iter().filter(|o| o.op == "replay").collect();
    assert!(
        !replay_ops.is_empty(),
        "provenance should contain a 'replay' operation"
    );

    let replay_op = &replay_ops[0];
    assert!(
        replay_op.detail["run_id"].as_str().is_some(),
        "replay op should have run_id in detail"
    );
    assert!(
        replay_op.detail["reset_tasks"].is_array(),
        "replay op should have reset_tasks in detail"
    );
    let reset_tasks = replay_op.detail["reset_tasks"].as_array().unwrap();
    assert!(
        reset_tasks.iter().any(|t| t == "p1"),
        "reset_tasks should include p1"
    );
}

// ===========================================================================
// 12. --plan-only produces no side effects
// ===========================================================================

#[test]
fn test_replay_plan_only_no_side_effects() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("x1", "Failed task", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    t1.blocks = vec!["x2".to_string()];
    let mut t2 = make_task("x2", "Dependent", Status::Done);
    t2.blocked_by = vec!["x1".to_string()];
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Run with --plan-only
    let output = wg_ok(&wg_dir, &["replay", "--failed-only", "--plan-only"]);
    assert!(
        output.contains("dry run") || output.contains("plan"),
        "plan-only output should mention dry run: {}",
        output
    );

    // Verify graph is unchanged
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(
        graph.get_task("x1").unwrap().status,
        Status::Failed,
        "x1 should still be Failed after plan-only"
    );
    assert_eq!(
        graph.get_task("x2").unwrap().status,
        Status::Done,
        "x2 should still be Done after plan-only"
    );

    // Verify no snapshot was created
    let runs_dir = wg_dir.join("runs");
    if runs_dir.exists() {
        let entries: Vec<_> = fs::read_dir(&runs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.is_empty(),
            "no run snapshots should exist after plan-only"
        );
    }

    // Verify no provenance entries were created for replay
    let ops = workgraph::provenance::read_all_operations(&wg_dir).unwrap();
    let replay_ops: Vec<_> = ops.iter().filter(|o| o.op == "replay").collect();
    assert!(
        replay_ops.is_empty(),
        "no replay provenance should exist after plan-only"
    );
}

// ===========================================================================
// 12b. --plan-only JSON output
// ===========================================================================

#[test]
fn test_replay_plan_only_json_output() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("y1", "Failed", Status::Failed);
    let t2 = make_task("y2", "Done", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    let output = wg_json(&wg_dir, &["replay", "--failed-only", "--plan-only"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

    assert_eq!(parsed["plan_only"], true);
    assert_eq!(parsed["run_id"], "(dry run)");
    assert!(parsed["reset_tasks"].is_array());
    let reset = parsed["reset_tasks"].as_array().unwrap();
    assert!(
        reset.iter().any(|t| t == "y1"),
        "plan should include y1 in reset_tasks"
    );
}

// ===========================================================================
// 13. --below-score with evaluation data
// ===========================================================================

#[test]
fn test_replay_below_score_with_evaluations() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("scored-high", "High score task", Status::Done);
    let t2 = make_task("scored-low", "Low score task", Status::Done);
    let t3 = make_task("no-score", "No score task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2, t3]);

    // Create evaluation files directly in agency/ dir
    // (load_all_evaluations_or_warn scans the agency dir for *.json)
    let eval_dir = wg_dir.join("agency");
    fs::create_dir_all(&eval_dir).unwrap();

    // High score evaluation
    let eval_high = serde_json::json!({
        "id": "eval-001",
        "task_id": "scored-high",
        "agent_id": "agent-1",
        "role_id": "implementer",
        "motivation_id": "quality",
        "score": 0.95,
        "dimensions": {},
        "notes": "Great work",
        "evaluator": "human",
        "timestamp": "2026-02-18T12:00:00Z"
    });
    fs::write(
        eval_dir.join("eval-001.json"),
        serde_json::to_string_pretty(&eval_high).unwrap(),
    )
    .unwrap();

    // Low score evaluation
    let eval_low = serde_json::json!({
        "id": "eval-002",
        "task_id": "scored-low",
        "agent_id": "agent-2",
        "role_id": "implementer",
        "motivation_id": "quality",
        "score": 0.3,
        "dimensions": {},
        "notes": "Needs improvement",
        "evaluator": "human",
        "timestamp": "2026-02-18T12:00:00Z"
    });
    fs::write(
        eval_dir.join("eval-002.json"),
        serde_json::to_string_pretty(&eval_low).unwrap(),
    )
    .unwrap();

    // Run replay --below-score 0.7
    wg_ok(&wg_dir, &["replay", "--below-score", "0.7"]);

    let graph = load_wg_graph(&wg_dir);
    // scored-high (0.95) should be preserved (score >= 0.7)
    // But wait — by default keep_done_threshold is 0.9, and scored-high scores 0.95 >= 0.9
    // The --below-score filter determines the seed set. scored-high has score 0.95 >= 0.7,
    // so it's NOT in the seed set. scored-low (0.3 < 0.7) IS in the seed set.
    // no-score is terminal with no eval => also in the seed set.
    assert_eq!(
        graph.get_task("scored-high").unwrap().status,
        Status::Done,
        "scored-high (0.95) should be preserved (above threshold)"
    );
    assert_eq!(
        graph.get_task("scored-low").unwrap().status,
        Status::Open,
        "scored-low (0.3) should be reset (below threshold)"
    );
    assert_eq!(
        graph.get_task("no-score").unwrap().status,
        Status::Open,
        "no-score (no eval) should be reset"
    );
}

// ===========================================================================
// 14. --subgraph scopes correctly
// ===========================================================================

#[test]
fn test_replay_subgraph_scopes_correctly() {
    let tmp = TempDir::new().unwrap();
    // Subgraph: root -> child (both failed)
    // Outside: unrelated (also failed, but should NOT be reset)
    let mut root = make_task("sg-root", "Root", Status::Failed);
    root.blocks = vec!["sg-child".to_string()];
    root.failure_reason = Some("err".to_string());
    let mut child = make_task("sg-child", "Child", Status::Failed);
    child.blocked_by = vec!["sg-root".to_string()];
    child.failure_reason = Some("err".to_string());
    let mut unrelated = make_task("outside", "Outside subgraph", Status::Failed);
    unrelated.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![root, child, unrelated]);

    wg_ok(
        &wg_dir,
        &[
            "replay",
            "--failed-only",
            "--subgraph",
            "sg-root",
            "--model",
            "opus",
        ],
    );

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(
        graph.get_task("sg-root").unwrap().status,
        Status::Open,
        "sg-root should be reset (in subgraph, failed)"
    );
    assert_eq!(
        graph.get_task("sg-child").unwrap().status,
        Status::Open,
        "sg-child should be reset (in subgraph, failed)"
    );
    assert_eq!(
        graph.get_task("outside").unwrap().status,
        Status::Failed,
        "outside should NOT be reset (not in subgraph)"
    );
}

// ===========================================================================
// Extra: Multiple replays increment run IDs
// ===========================================================================

#[test]
fn test_multiple_replays_increment_run_ids() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("r1", "Task R1", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // First replay
    wg_ok(&wg_dir, &["replay", "--failed-only"]);
    let output1 = wg_json(&wg_dir, &["runs", "list"]);
    let runs1: serde_json::Value = serde_json::from_str(&output1).unwrap();
    assert_eq!(runs1.as_array().unwrap().len(), 1);
    assert_eq!(runs1[0]["id"], "run-001");

    // Reset it back to failed for another replay
    {
        let mut graph = load_wg_graph(&wg_dir);
        let task = graph.get_task_mut("r1").unwrap();
        task.status = Status::Failed;
        task.failure_reason = Some("err again".to_string());
        save_graph(&graph, &wg_dir.join("graph.jsonl")).unwrap();
    }

    // Second replay
    wg_ok(&wg_dir, &["replay", "--failed-only"]);
    let output2 = wg_json(&wg_dir, &["runs", "list"]);
    let runs2: serde_json::Value = serde_json::from_str(&output2).unwrap();
    assert_eq!(runs2.as_array().unwrap().len(), 2);
    assert_eq!(runs2[1]["id"], "run-002");
}

// ===========================================================================
// Extra: Trace with ops-only and full modes
// ===========================================================================

#[test]
fn test_trace_ops_only_mode() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("ops-task", "Ops test", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create a replay to generate provenance entries
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    let output = wg_ok(&wg_dir, &["trace", "ops-task", "--ops-only"]);
    // Should show ops or "No operations" — the replay op is task_id=None,
    // but there should be no per-task ops unless we add some
    assert!(
        output.contains("Operations") || output.contains("operations"),
        "ops-only output should mention operations: {}",
        output
    );
}

#[test]
fn test_trace_full_mode() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("full-task", "Full trace test", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let output = wg_ok(&wg_dir, &["trace", "full-task", "--full"]);
    assert!(
        output.contains("Trace: full-task"),
        "full mode output should contain task header: {}",
        output
    );
}

// ===========================================================================
// Extra: Replay with --tasks flag (explicit task list)
// ===========================================================================

#[test]
fn test_replay_explicit_tasks() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("e1", "Task E1", Status::Done);
    t1.blocks = vec!["e2".to_string()];
    let mut t2 = make_task("e2", "Task E2", Status::Done);
    t2.blocked_by = vec!["e1".to_string()];
    let t3 = make_task("e3", "Task E3", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2, t3]);

    // Only reset e1 (and e2 as transitive dependent), but NOT e3
    wg_ok(&wg_dir, &["replay", "--tasks", "e1"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("e1").unwrap().status, Status::Open);
    assert_eq!(
        graph.get_task("e2").unwrap().status,
        Status::Open,
        "e2 should be reset as transitive dependent of e1"
    );
    assert_eq!(
        graph.get_task("e3").unwrap().status,
        Status::Done,
        "e3 should be preserved (not in explicit list or dependents)"
    );
}

// ===========================================================================
// Extra: Replay with no matching tasks does nothing
// ===========================================================================

#[test]
fn test_replay_no_matching_tasks() {
    let tmp = TempDir::new().unwrap();
    // All open — --failed-only should find nothing
    let t1 = make_task("n1", "Open task", Status::Open);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let output = wg_ok(&wg_dir, &["replay", "--failed-only"]);
    assert!(
        output.contains("No tasks match") || output.contains("Nothing to replay"),
        "should report no matching tasks: {}",
        output
    );

    // No runs should be created
    let runs_dir = wg_dir.join("runs");
    if runs_dir.exists() {
        let entries: Vec<_> = fs::read_dir(&runs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "no run snapshots should exist");
    }
}

// ===========================================================================
// Extra: Replay JSON output
// ===========================================================================

#[test]
fn test_replay_json_output() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("j1", "JSON test", Status::Failed);
    let t2 = make_task("j2", "Preserved", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    let output = wg_json(&wg_dir, &["replay", "--failed-only", "--model", "opus"]);
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();

    assert!(parsed["run_id"].as_str().unwrap().starts_with("run-"));
    assert_eq!(parsed["model"], "opus");
    assert_eq!(parsed["plan_only"], false);
    let reset = parsed["reset_tasks"].as_array().unwrap();
    assert!(reset.iter().any(|t| t == "j1"));
    let preserved = parsed["preserved_tasks"].as_array().unwrap();
    assert!(preserved.iter().any(|t| t == "j2"));
}
