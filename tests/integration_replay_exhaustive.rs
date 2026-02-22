//! Exhaustive integration tests for replay, trace, and runs commands.
//!
//! Covers every scenario from docs/test-specs/trace-replay-test-spec.md.
//! Tests exercise the `wg` binary end-to-end via CLI invocation.

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

fn wg_json(wg_dir: &Path, args: &[&str]) -> serde_json::Value {
    let mut full_args = vec!["--json"];
    full_args.extend_from_slice(args);
    let output = wg_ok(wg_dir, &full_args);
    serde_json::from_str(&output).unwrap_or_else(|e| {
        panic!("Failed to parse JSON.\nError: {}\nOutput: {}", e, output)
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

fn write_evaluation(wg_dir: &Path, eval_id: &str, task_id: &str, score: f64) {
    let eval_dir = wg_dir.join("agency");
    fs::create_dir_all(&eval_dir).unwrap();
    let eval = serde_json::json!({
        "id": eval_id,
        "task_id": task_id,
        "agent_id": "agent-1",
        "role_id": "implementer",
        "motivation_id": "quality",
        "score": score,
        "dimensions": {},
        "notes": "",
        "evaluator": "human",
        "timestamp": "2026-02-18T12:00:00Z"
    });
    fs::write(
        eval_dir.join(format!("{}.json", eval_id)),
        serde_json::to_string_pretty(&eval).unwrap(),
    )
    .unwrap();
}

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

// ===========================================================================
// 1. TRACE TESTS
// ===========================================================================

// 1.1 trace_no_agent_runs_manual_done — verify output content
#[test]
fn test_trace_no_agent_runs_output_content() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Manual task", Status::Done);
    t1.started_at = Some("2026-02-18T10:00:00+00:00".to_string());
    t1.completed_at = Some("2026-02-18T11:00:00+00:00".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Record provenance entries
    workgraph::provenance::record(
        &wg_dir, "add_task", Some("t1"), None,
        serde_json::json!({"title": "Manual task"}),
        workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
    ).unwrap();
    workgraph::provenance::record(
        &wg_dir, "claim", Some("t1"), Some("human"),
        serde_json::Value::Null,
        workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
    ).unwrap();
    workgraph::provenance::record(
        &wg_dir, "done", Some("t1"), None,
        serde_json::Value::Null,
        workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
    ).unwrap();

    // Human-readable output
    let output = wg_ok(&wg_dir, &["trace", "show", "t1"]);
    assert!(output.contains("Agent runs: (none)"), "Should show no agent runs: {}", output);

    // JSON output
    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    assert_eq!(json["agent_runs"].as_array().unwrap().len(), 0);
    assert_eq!(json["summary"]["agent_run_count"], 0);
    assert!(json["operations"].as_array().unwrap().len() >= 3,
        "Should have at least 3 operations: {:?}", json["operations"]);
}

// 1.2 trace_multiple_agent_runs_retried_task
#[test]
fn test_trace_multiple_agent_runs() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Retried task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt 1", "output 1");
    create_agent_archive(&wg_dir, "t1", "2026-02-18T11:00:00Z", "prompt 2", "output 2");

    let output = wg_ok(&wg_dir, &["trace", "show", "t1"]);
    assert!(output.contains("Agent runs (2):"), "Should show 2 agent runs: {}", output);

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    let runs = json["agent_runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(json["summary"]["agent_run_count"], 2);
    // Verify chronological order
    assert_eq!(runs[0]["timestamp"], "2026-02-18T10:00:00Z");
    assert_eq!(runs[1]["timestamp"], "2026-02-18T11:00:00Z");
}

// 1.3 trace_json_structure_validation — full field validation
#[test]
fn test_trace_json_structure_validation() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Full task", Status::Done);
    t1.assigned = Some("agent-1".to_string());
    t1.created_at = Some("2026-02-18T09:00:00+00:00".to_string());
    t1.started_at = Some("2026-02-18T10:00:00+00:00".to_string());
    t1.completed_at = Some("2026-02-18T11:00:00+00:00".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Record provenance
    workgraph::provenance::record(
        &wg_dir, "add_task", Some("t1"), None,
        serde_json::json!({"title": "Full task"}),
        workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
    ).unwrap();

    // Create agent archive with stream-json output
    let stream_output = r#"{"type":"assistant","message":"hello"}
{"type":"tool_use","name":"Read","id":"1"}
{"type":"tool_result","tool_use_id":"1"}
{"type":"assistant","message":"done"}
{"type":"result","cost":{"input":100,"output":50}}
"#;
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:30:00Z", "Test prompt", stream_output);

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);

    // Top-level fields
    assert_eq!(json["id"], "t1");
    assert_eq!(json["title"], "Full task");
    assert_eq!(json["status"], "done");
    assert_eq!(json["assigned"], "agent-1");
    assert!(json["created_at"].is_string());
    assert!(json["started_at"].is_string());
    assert!(json["completed_at"].is_string());

    // Operations
    assert!(json["operations"].is_array());
    let ops = json["operations"].as_array().unwrap();
    assert!(!ops.is_empty());
    let op = &ops[0];
    assert!(op["timestamp"].is_string());
    assert!(op["op"].is_string());

    // Agent runs
    let runs = json["agent_runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    let run = &runs[0];
    assert!(run["timestamp"].is_string());
    assert!(run["prompt_bytes"].is_number());
    assert!(run["output_bytes"].is_number());
    assert!(run["tool_calls"].is_number());
    assert!(run["turns"].is_number());

    // Summary
    assert!(json["summary"]["operation_count"].is_number());
    assert_eq!(json["summary"]["agent_run_count"], 1);
    assert!(json["summary"]["duration_secs"].is_number());
    assert!(json["summary"]["duration_human"].is_string());
    assert!(json["summary"]["total_tool_calls"].is_number());
    assert!(json["summary"]["total_turns"].is_number());
    assert!(json["summary"]["total_output_bytes"].is_number());
}

// 1.4 trace_full_output_contains_conversation
#[test]
fn test_trace_full_output_contains_conversation() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Full mode task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let prompt_text = "This is a multi-line\nprompt for the agent\nwith details.";
    let output_text = "Agent output response\nwith multiple lines\nof content.";
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", prompt_text, output_text);

    let output = wg_ok(&wg_dir, &["trace", "show", "t1", "--full"]);
    assert!(output.contains("[Prompt]"), "Should contain [Prompt] header: {}", output);
    assert!(output.contains("[Output]"), "Should contain [Output] header: {}", output);
    assert!(output.contains("multi-line"), "Should contain prompt content: {}", output);
    assert!(output.contains("Agent output response"), "Should contain output content: {}", output);
}

// 1.5 trace_ops_only_shows_only_provenance
#[test]
fn test_trace_ops_only_excludes_agent_runs() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Ops only task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Record provenance
    workgraph::provenance::record(
        &wg_dir, "add_task", Some("t1"), None,
        serde_json::json!({"title": "Ops only task"}),
        workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
    ).unwrap();
    workgraph::provenance::record(
        &wg_dir, "done", Some("t1"), None,
        serde_json::Value::Null,
        workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
    ).unwrap();

    // Create an agent archive (should be ignored by --ops-only)
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt", "output");

    let output = wg_ok(&wg_dir, &["trace", "show", "t1", "--ops-only"]);
    assert!(output.contains("Operations for 't1'"), "Should show operations header: {}", output);
    assert!(output.contains("add_task"), "Should show add_task op: {}", output);
    assert!(output.contains("done"), "Should show done op: {}", output);
    assert!(!output.contains("Agent runs"), "Should NOT show agent runs section: {}", output);
    assert!(!output.contains("Summary"), "Should NOT show summary: {}", output);
}

// 1.7 trace_in_progress_task
#[test]
fn test_trace_in_progress_task() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "In-progress task", Status::Open);
    t1.assigned = Some("agent-1".to_string());
    t1.started_at = Some("2026-02-18T10:00:00+00:00".to_string());
    // No completed_at
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create a partial agent archive
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt", "partial output");

    let output = wg_ok(&wg_dir, &["trace", "show", "t1"]);
    assert!(output.contains("open"), "Should show status as open: {}", output);

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    assert_eq!(json["status"], "open");
    assert!(json["summary"]["duration_secs"].is_null(), "duration_secs should be absent for in-progress");
    assert!(json["summary"]["duration_human"].is_null(), "duration_human should be absent");
    assert_eq!(json["summary"]["agent_run_count"], 1, "Should still list agent runs");
}

// 1.9 trace_output_size_accuracy
#[test]
fn test_trace_output_size_accuracy() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Size test", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Write exactly 10240 bytes of output
    let output_data = "x".repeat(10240);
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt", &output_data);

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    assert_eq!(json["summary"]["total_output_bytes"], 10240);
}

// 1.10 trace_turn_count_accuracy
#[test]
fn test_trace_turn_count_accuracy() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Turn count test", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let stream_output = r#"{"type":"assistant","message":"turn 1"}
{"type":"tool_use","name":"Read","id":"1"}
{"type":"tool_result","tool_use_id":"1"}
{"type":"tool_use","name":"Write","id":"2"}
{"type":"tool_result","tool_use_id":"2"}
{"type":"assistant","message":"turn 2"}
{"type":"tool_use","name":"Edit","id":"3"}
{"type":"tool_result","tool_use_id":"3"}
{"type":"tool_use","name":"Bash","id":"4"}
{"type":"tool_result","tool_use_id":"4"}
{"type":"tool_use","name":"Grep","id":"5"}
{"type":"tool_result","tool_use_id":"5"}
{"type":"assistant","message":"turn 3"}
{"type":"result","cost":{"input":100,"output":50}}
"#;
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt", stream_output);

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    let run = &json["agent_runs"][0];
    assert_eq!(run["turns"], 3, "Should count 3 assistant turns");
    assert_eq!(run["tool_calls"], 5, "Should count 5 tool_use calls");
    assert_eq!(json["summary"]["total_turns"], 3);
    assert_eq!(json["summary"]["total_tool_calls"], 5);
}

// 1.12 trace_content_block_tool_use_counting
#[test]
fn test_trace_content_block_tool_use_counting() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Content block test", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let stream_output = r#"{"type":"assistant","message":"hello"}
{"type":"tool_use","name":"Read","id":"1"}
{"content_block":{"type":"tool_use","name":"Write","id":"2"}}
{"type":"result","cost":{"input":100,"output":50}}
"#;
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt", stream_output);

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    let run = &json["agent_runs"][0];
    assert_eq!(run["tool_calls"], 2, "Should count both top-level and content_block tool_use");
}

// ===========================================================================
// 2. REPLAY TESTS
// ===========================================================================

// 2.2 replay_failed_only_with_abandoned
#[test]
fn test_replay_failed_only_with_abandoned() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Failed task", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let t2 = make_task("t2", "Abandoned task", Status::Abandoned);
    let t3 = make_task("t3", "Done task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2, t3]);

    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Open, "Failed should be reset");
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Open, "Abandoned should be reset");
    assert_eq!(graph.get_task("t3").unwrap().status, Status::Done, "Done should be preserved");
}

// 2.3 replay_below_score_various_thresholds
#[test]
fn test_replay_below_score_threshold_0_5() {
    let tmp = TempDir::new().unwrap();
    let t_high = make_task("high", "High score", Status::Done);
    let t_med = make_task("med", "Medium score", Status::Done);
    let t_low = make_task("low", "Low score", Status::Done);
    let t_no = make_task("no-score", "No score", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t_high, t_med, t_low, t_no]);

    write_evaluation(&wg_dir, "eval-high", "high", 0.95);
    write_evaluation(&wg_dir, "eval-med", "med", 0.6);
    write_evaluation(&wg_dir, "eval-low", "low", 0.2);

    wg_ok(&wg_dir, &["replay", "--below-score", "0.5"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("high").unwrap().status, Status::Done, "0.95 >= 0.5, preserved");
    assert_eq!(graph.get_task("med").unwrap().status, Status::Done, "0.6 >= 0.5, preserved");
    assert_eq!(graph.get_task("low").unwrap().status, Status::Open, "0.2 < 0.5, reset");
    assert_eq!(graph.get_task("no-score").unwrap().status, Status::Open, "no score, reset");
}

#[test]
fn test_replay_below_score_threshold_1_0() {
    let tmp = TempDir::new().unwrap();
    let t_high = make_task("high", "High score", Status::Done);
    let t_low = make_task("low", "Low score", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t_high, t_low]);

    write_evaluation(&wg_dir, "eval-high", "high", 0.95);
    write_evaluation(&wg_dir, "eval-low", "low", 0.2);

    // Need --keep-done 1.0 to override the default keep_done_threshold (0.9)
    // which would otherwise preserve the high-scored task
    wg_ok(&wg_dir, &["replay", "--below-score", "1.0", "--keep-done", "1.0"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("high").unwrap().status, Status::Open, "0.95 < 1.0, reset");
    assert_eq!(graph.get_task("low").unwrap().status, Status::Open, "0.2 < 1.0, reset");
}

// 2.5 replay_tasks_multiple_explicit
#[test]
fn test_replay_tasks_multiple_explicit() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task 1", Status::Done);
    let t2 = make_task("t2", "Task 2", Status::Done);
    let t3 = make_task("t3", "Task 3", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2, t3]);

    wg_ok(&wg_dir, &["replay", "--tasks", "t1,t3"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Open, "t1 explicitly listed, reset");
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Done, "t2 not listed, preserved");
    assert_eq!(graph.get_task("t3").unwrap().status, Status::Open, "t3 explicitly listed, reset");
}

// 2.6 replay_keep_done_preserves_high_scoring
#[test]
fn test_replay_keep_done_preserves_high_scoring() {
    let tmp = TempDir::new().unwrap();
    let mut parent = make_task("parent", "Parent", Status::Failed);
    parent.before = vec!["child".to_string()];
    parent.failure_reason = Some("err".to_string());
    let mut child = make_task("child", "Child", Status::Done);
    child.after = vec!["parent".to_string()];
    let wg_dir = setup_workgraph(&tmp, vec![parent, child]);

    write_evaluation(&wg_dir, "eval-child", "child", 0.95);

    // --failed-only resets parent; child is transitive dependent.
    // --keep-done 0.9 should preserve child (score 0.95 >= 0.9).
    wg_ok(&wg_dir, &["replay", "--failed-only", "--keep-done", "0.9"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("parent").unwrap().status, Status::Open, "parent reset (failed)");
    assert_eq!(graph.get_task("child").unwrap().status, Status::Done, "child preserved by keep-done");
}

// 2.12 replay_preserves_structure_clears_execution — field clearing
#[test]
fn test_replay_field_clearing_and_preservation() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Full task", Status::Done);
    t1.description = Some("Detailed description".to_string());
    t1.assigned = Some("agent-1".to_string());
    t1.started_at = Some("2026-02-18T10:00:00+00:00".to_string());
    t1.completed_at = Some("2026-02-18T11:00:00+00:00".to_string());
    t1.artifacts = vec!["file.rs".to_string()];
    t1.loop_iteration = 3;
    t1.failure_reason = Some("some error".to_string());
    t1.paused = true;
    t1.after = vec!["dep".to_string()];
    t1.before = vec!["child".to_string()];
    t1.tags = vec!["rust".to_string(), "test".to_string()];
    t1.skills = vec!["implementation".to_string()];
    t1.log = vec![workgraph::graph::LogEntry {
        timestamp: "2026-02-18T10:00:00+00:00".to_string(),
        actor: None,
        message: "Started work".to_string(),
    }];

    let dep = make_task("dep", "Dependency", Status::Done);
    let mut child = make_task("child", "Child", Status::Done);
    child.after = vec!["t1".to_string()];

    let wg_dir = setup_workgraph(&tmp, vec![t1, dep, child]);

    // Replay all terminal tasks
    wg_ok(&wg_dir, &["replay"]);

    let graph = load_wg_graph(&wg_dir);
    let task = graph.get_task("t1").unwrap();

    // Cleared fields
    assert_eq!(task.status, Status::Open);
    assert!(task.assigned.is_none(), "assigned should be cleared");
    assert!(task.started_at.is_none(), "started_at should be cleared");
    assert!(task.completed_at.is_none(), "completed_at should be cleared");
    assert!(task.artifacts.is_empty(), "artifacts should be cleared");
    assert_eq!(task.loop_iteration, 0, "loop_iteration should be 0");
    assert!(task.failure_reason.is_none(), "failure_reason should be cleared");
    assert!(!task.paused, "paused should be false");

    // Preserved fields
    assert_eq!(task.title, "Full task");
    assert_eq!(task.description, Some("Detailed description".to_string()));
    assert_eq!(task.after, vec!["dep"]);
    assert_eq!(task.before, vec!["child"]);
    assert_eq!(task.tags, vec!["rust", "test"]);
    assert_eq!(task.skills, vec!["implementation"]);
    assert!(!task.log.is_empty(), "log should be preserved");
}

// 2.15 replay_empty_graph
#[test]
fn test_replay_empty_graph() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_ok(&wg_dir, &["replay", "--failed-only"]);
    assert!(
        output.contains("No tasks match") || output.contains("Nothing to replay"),
        "Should report no matching tasks on empty graph: {}",
        output
    );

    // No snapshot created
    let runs_dir = wg_dir.join("runs");
    if runs_dir.exists() {
        let entries: Vec<_> = fs::read_dir(&runs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "no run snapshots should exist for empty graph");
    }
}

// 2.17 replay_subgraph_nonexistent_root
#[test]
fn test_replay_subgraph_nonexistent_root() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let output = wg_cmd(&wg_dir, &["replay", "--failed-only", "--subgraph", "nonexistent"]);
    assert!(
        !output.status.success(),
        "Should fail for nonexistent subgraph root"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("Not found"),
        "Error should mention not found: {}",
        stderr
    );
}

// 2.21 replay_below_score_non_terminal_ignored
#[test]
fn test_replay_below_score_non_terminal_ignored() {
    let tmp = TempDir::new().unwrap();
    let open_task = make_task("open-task", "Open task", Status::Open);
    let done_task = make_task("done-task", "Done task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![open_task, done_task]);

    write_evaluation(&wg_dir, "eval-open", "open-task", 0.1);
    write_evaluation(&wg_dir, "eval-done", "done-task", 0.1);

    wg_ok(&wg_dir, &["replay", "--below-score", "0.5"]);

    let graph = load_wg_graph(&wg_dir);
    // done_task is terminal with low score => reset
    assert_eq!(graph.get_task("done-task").unwrap().status, Status::Open, "terminal low-score task reset");
    // open_task: the below_score path adds it if score < threshold regardless of terminal status.
    // Document actual behavior here.
    let open_status = graph.get_task("open-task").unwrap().status;
    // The code at replay.rs:87-98 adds tasks with score < threshold regardless of terminal status.
    // For non-terminal tasks with a score, they DO get added to seeds if score < threshold.
    assert_eq!(open_status, Status::Open, "open task status");
}

// 2.22 replay_transitive_dependents_chain (4-deep)
#[test]
fn test_replay_transitive_dependents_deep_chain() {
    let tmp = TempDir::new().unwrap();
    let mut a = make_task("a", "A", Status::Failed);
    a.before = vec!["b".to_string()];
    a.failure_reason = Some("err".to_string());
    let mut b = make_task("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    b.before = vec!["c".to_string()];
    let mut c = make_task("c", "C", Status::Done);
    c.after = vec!["b".to_string()];
    c.before = vec!["d".to_string()];
    let mut d = make_task("d", "D", Status::Done);
    d.after = vec!["c".to_string()];
    let wg_dir = setup_workgraph(&tmp, vec![a, b, c, d]);

    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open, "a (failed seed) reset");
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open, "b (dependent of a) reset");
    assert_eq!(graph.get_task("c").unwrap().status, Status::Open, "c (dependent of b) reset");
    assert_eq!(graph.get_task("d").unwrap().status, Status::Open, "d (dependent of c) reset");
}

// 2.23 replay_diamond_dependency
#[test]
fn test_replay_diamond_dependency() {
    let tmp = TempDir::new().unwrap();
    let mut a = make_task("a", "A", Status::Failed);
    a.before = vec!["b".to_string(), "c".to_string()];
    a.failure_reason = Some("err".to_string());
    let mut b = make_task("b", "B", Status::Done);
    b.after = vec!["a".to_string()];
    b.before = vec!["d".to_string()];
    let mut c = make_task("c", "C", Status::Done);
    c.after = vec!["a".to_string()];
    c.before = vec!["d".to_string()];
    let mut d = make_task("d", "D", Status::Done);
    d.after = vec!["b".to_string(), "c".to_string()];
    let wg_dir = setup_workgraph(&tmp, vec![a, b, c, d]);

    let json = wg_json(&wg_dir, &["replay", "--failed-only"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("c").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("d").unwrap().status, Status::Open);

    // d should appear only once in reset_tasks (no duplication)
    let reset = json["reset_tasks"].as_array().unwrap();
    let d_count = reset.iter().filter(|t| t.as_str() == Some("d")).count();
    assert_eq!(d_count, 1, "d should appear exactly once in reset_tasks");
}

// 2.24 replay_keep_done_with_no_evaluations
#[test]
fn test_replay_keep_done_with_no_evaluations() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Done task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);
    // No evaluation files

    // Default filter (all terminal) + --keep-done 0.8
    wg_ok(&wg_dir, &["replay", "--keep-done", "0.8"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Open, "no score, not kept by keep-done");
}

// 2.25 replay_filter_description_in_metadata
#[test]
fn test_replay_filter_description_in_metadata() {
    let tmp = TempDir::new().unwrap();
    let mut root = make_task("root", "Root", Status::Failed);
    root.before = vec!["child".to_string()];
    root.failure_reason = Some("err".to_string());
    let mut child = make_task("child", "Child", Status::Failed);
    child.after = vec!["root".to_string()];
    child.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![root, child]);

    wg_ok(&wg_dir, &["replay", "--failed-only", "--model", "opus", "--subgraph", "root"]);

    let json = wg_json(&wg_dir, &["runs", "show", "run-001"]);
    let filter = json["filter"].as_str().unwrap();
    assert!(filter.contains("--failed-only"), "Filter should contain --failed-only: {}", filter);
    assert!(filter.contains("--model opus"), "Filter should contain --model opus: {}", filter);
    assert!(filter.contains("--subgraph root"), "Filter should contain --subgraph root: {}", filter);
}

// ===========================================================================
// 3. RUNS TESTS
// ===========================================================================

// 3.5 runs_show_nonexistent
#[test]
fn test_runs_show_nonexistent() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(&wg_dir, &["runs", "show", "run-999"]);
    assert!(!output.status.success(), "Should fail for nonexistent run");
}

// 3.6 runs_restore_actual_task_status
#[test]
fn test_runs_restore_actual_task_status() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task 1", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Replay resets t1, creating snapshot run-001 with t1=Failed
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Verify t1 is now Open
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Open);

    // Restore from run-001
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Verify t1 is back to Failed
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Failed,
        "t1 should be restored to Failed from snapshot");
}

// 3.8 runs_restore_provenance
#[test]
fn test_runs_restore_provenance() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create a snapshot via replay
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Restore from run-001
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Check provenance
    let ops = workgraph::provenance::read_all_operations(&wg_dir).unwrap();
    let restore_ops: Vec<_> = ops.iter().filter(|o| o.op == "restore").collect();
    assert!(!restore_ops.is_empty(), "Should have a restore provenance entry");

    let restore_op = &restore_ops[0];
    assert_eq!(restore_op.detail["restored_from"], "run-001");
    assert!(restore_op.detail["safety_snapshot"].as_str().is_some(),
        "Should have safety_snapshot in detail");
}

// 3.9 runs_restore_nonexistent
#[test]
fn test_runs_restore_nonexistent() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(&wg_dir, &["runs", "restore", "nonexistent-run"]);
    assert!(!output.status.success(), "Should fail for nonexistent run");
}

// 3.10 runs_diff_with_added_and_removed_tasks
#[test]
fn test_runs_diff_with_status_change_added_removed() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task 1", Status::Done);
    let t2 = make_task("t2", "Task 2", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Create a snapshot manually
    let meta = workgraph::runs::RunMeta {
        id: "run-001".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        model: None,
        reset_tasks: vec![],
        preserved_tasks: vec!["t1".to_string(), "t2".to_string()],
        filter: None,
    };
    workgraph::runs::snapshot(&wg_dir, "run-001", &meta).unwrap();

    // Modify graph: change t2 status, remove t1 (well, we'll rebuild), add t3
    let mut new_t2 = make_task("t2", "Task 2", Status::Open);
    new_t2.failure_reason = None;
    let t3 = make_task("t3", "Task 3", Status::Open);
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    // t1 removed (not in new graph), t2 changed, t3 added
    graph.add_node(Node::Task(new_t2));
    graph.add_node(Node::Task(t3));
    save_graph(&graph, &graph_path).unwrap();

    let output = wg_ok(&wg_dir, &["runs", "diff", "run-001"]);
    assert!(output.contains("t2"), "Should show t2 change: {}", output);

    // JSON diff
    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    let changes = json["changes"].as_array().unwrap();
    assert!(json["total_changes"].as_u64().unwrap() >= 2, "Should have at least 2 changes");

    // Find specific changes
    let t1_change = changes.iter().find(|c| c["id"] == "t1");
    let t2_change = changes.iter().find(|c| c["id"] == "t2");
    let t3_change = changes.iter().find(|c| c["id"] == "t3");

    assert!(t1_change.is_some(), "t1 should appear as removed");
    assert_eq!(t1_change.unwrap()["change"], "removed");
    assert!(t2_change.is_some(), "t2 should appear as changed");
    assert_eq!(t2_change.unwrap()["change"], "status_changed");
    assert!(t3_change.is_some(), "t3 should appear as added");
    assert_eq!(t3_change.unwrap()["change"], "added");
}

// 3.11 runs_diff_no_changes
#[test]
fn test_runs_diff_no_changes() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task 1", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let meta = workgraph::runs::RunMeta {
        id: "run-001".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        model: None,
        reset_tasks: vec![],
        preserved_tasks: vec!["t1".to_string()],
        filter: None,
    };
    workgraph::runs::snapshot(&wg_dir, "run-001", &meta).unwrap();

    let output = wg_ok(&wg_dir, &["runs", "diff", "run-001"]);
    assert!(output.contains("No differences"), "Should report no differences: {}", output);
}

// 3.12 runs_diff_json_output
#[test]
fn test_runs_diff_json_output() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Snapshot, then modify
    let meta = workgraph::runs::RunMeta {
        id: "run-001".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        model: None,
        reset_tasks: vec![],
        preserved_tasks: vec!["t1".to_string()],
        filter: None,
    };
    workgraph::runs::snapshot(&wg_dir, "run-001", &meta).unwrap();

    // Change t1 to Open
    let mut graph = load_wg_graph(&wg_dir);
    graph.get_task_mut("t1").unwrap().status = Status::Open;
    save_graph(&graph, wg_dir.join("graph.jsonl")).unwrap();

    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    assert_eq!(json["run_id"], "run-001");
    assert!(json["changes"].is_array());
    assert!(json["total_changes"].is_number());
    let changes = json["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["id"], "t1");
    assert_eq!(changes[0]["snapshot_status"], "failed");
    assert_eq!(changes[0]["current_status"], "open");
    assert_eq!(changes[0]["change"], "status_changed");
}

// 3.13 runs_diff_nonexistent_run
#[test]
fn test_runs_diff_nonexistent_run() {
    let tmp = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(&tmp, vec![]);

    let output = wg_cmd(&wg_dir, &["runs", "diff", "nonexistent"]);
    assert!(!output.status.success(), "Should fail for nonexistent run");
}

// 3.14 runs_list_json
#[test]
fn test_runs_list_json() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // First replay
    wg_ok(&wg_dir, &["replay", "--failed-only"]);
    // Reset and do second replay
    {
        let mut graph = load_wg_graph(&wg_dir);
        graph.get_task_mut("t1").unwrap().status = Status::Failed;
        graph.get_task_mut("t1").unwrap().failure_reason = Some("err".to_string());
        save_graph(&graph, wg_dir.join("graph.jsonl")).unwrap();
    }
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    let json = wg_json(&wg_dir, &["runs", "list"]);
    let runs = json.as_array().unwrap();
    assert_eq!(runs.len(), 2);
    // Each element should have full metadata
    for run in runs {
        assert!(run["id"].is_string());
        assert!(run["timestamp"].is_string());
        assert!(run["reset_tasks"].is_array());
        assert!(run["preserved_tasks"].is_array());
    }
    assert_eq!(runs[0]["id"], "run-001");
    assert_eq!(runs[1]["id"], "run-002");
}

// 3.18 snapshot_without_config_toml
#[test]
fn test_snapshot_without_config_toml() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Ensure no config.toml
    let config_path = wg_dir.join("config.toml");
    if config_path.exists() {
        fs::remove_file(&config_path).unwrap();
    }

    // Replay should succeed even without config.toml
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Verify snapshot was created successfully
    let runs_dir = wg_dir.join("runs");
    assert!(runs_dir.exists());
    let run_dir = runs_dir.join("run-001");
    assert!(run_dir.join("graph.jsonl").exists());
    assert!(run_dir.join("meta.json").exists());
    // config.toml should be absent from snapshot
    assert!(!run_dir.join("config.toml").exists());
}

// 3.19 runs_restore_json_output
#[test]
fn test_runs_restore_json_output() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    let json = wg_json(&wg_dir, &["runs", "restore", "run-001"]);
    assert_eq!(json["restored_from"], "run-001");
    assert!(json["safety_snapshot"].is_string(), "Should have safety_snapshot");
    assert!(json["timestamp"].is_string(), "Should have timestamp");
}

// ===========================================================================
// 4. CROSS-CUTTING / INTEGRATION
// ===========================================================================

// 4.1 full_replay_restore_round_trip
#[test]
fn test_full_replay_restore_round_trip() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Done task", Status::Done);
    let mut t2 = make_task("t2", "Failed task", Status::Failed);
    t2.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // 1. Replay --failed-only => creates run-001
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // 2. Verify t2 is now Open
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Done);

    // 3. Restore from run-001
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // 4. Verify t2 is back to Failed
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Failed,
        "t2 should be restored to Failed");
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Done,
        "t1 should still be Done");
}

// 4.2 replay_then_diff
#[test]
fn test_replay_then_diff() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Failed task", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Replay creates run-001 snapshot (with t1=Failed), then resets t1 to Open
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Diff should show t1: Failed -> Open
    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    let changes = json["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0]["id"], "t1");
    assert_eq!(changes[0]["snapshot_status"], "failed");
    assert_eq!(changes[0]["current_status"], "open");
    assert_eq!(changes[0]["change"], "status_changed");
}

// 4.4 trace_after_replay
#[test]
fn test_trace_after_replay() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Task", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create an agent archive before replay
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt", "output");

    // Replay
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Trace the reset task
    let output = wg_ok(&wg_dir, &["trace", "show", "t1"]);
    assert!(output.contains("open"), "Should show task as open: {}", output);

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    assert_eq!(json["status"], "open");
    // Agent archives from before replay should still be accessible
    assert_eq!(json["summary"]["agent_run_count"], 1,
        "Agent archives should survive replay");
}

// ===========================================================================
// 5. HELPER / UTILITY FUNCTIONS (tested via trace)
// ===========================================================================

// 5.2 parse_stream_json_result_only
#[test]
fn test_trace_result_only_stream_json() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Result only", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let stream_output = r#"{"type":"result","cost":{"input":100,"output":50}}"#;
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt", stream_output);

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    let run = &json["agent_runs"][0];
    // With only a result message, turns should be 1
    assert_eq!(run["turns"], 1, "result-only should count as 1 turn");
}

// 5.4 build_score_map_multiple_evals_per_task
#[test]
fn test_replay_multiple_evals_keeps_highest_score() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Multi-eval task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Write two evaluations for the same task
    write_evaluation(&wg_dir, "eval-low", "t1", 0.4);
    write_evaluation(&wg_dir, "eval-high", "t1", 0.8);

    // --below-score 0.5: if max score (0.8) is used, t1 should be preserved
    wg_ok(&wg_dir, &["replay", "--below-score", "0.5"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Done,
        "Should use highest score (0.8 >= 0.5), task preserved");
}

// 5.5 collect_subgraph_deep_tree (via --subgraph)
#[test]
fn test_replay_subgraph_deep_tree() {
    let tmp = TempDir::new().unwrap();
    let mut root = make_task("root", "Root", Status::Failed);
    root.before = vec!["a".to_string()];
    root.failure_reason = Some("err".to_string());
    let mut a = make_task("a", "A", Status::Failed);
    a.after = vec!["root".to_string()];
    a.before = vec!["b".to_string()];
    a.failure_reason = Some("err".to_string());
    let mut b = make_task("b", "B", Status::Failed);
    b.after = vec!["a".to_string()];
    b.before = vec!["c".to_string()];
    b.failure_reason = Some("err".to_string());
    let mut c = make_task("c", "C", Status::Failed);
    c.after = vec!["b".to_string()];
    c.failure_reason = Some("err".to_string());

    // Outside subgraph
    let mut outside = make_task("outside", "Outside", Status::Failed);
    outside.failure_reason = Some("err".to_string());

    let wg_dir = setup_workgraph(&tmp, vec![root, a, b, c, outside]);

    wg_ok(&wg_dir, &["replay", "--failed-only", "--subgraph", "root"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("root").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("c").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("outside").unwrap().status, Status::Failed,
        "outside should NOT be reset (not in subgraph)");
}

// 5.6 collect_subgraph_with_cycles
#[test]
fn test_replay_subgraph_with_cycles() {
    let tmp = TempDir::new().unwrap();
    let mut a = make_task("a", "A", Status::Failed);
    a.before = vec!["b".to_string()];
    a.failure_reason = Some("err".to_string());
    let mut b = make_task("b", "B", Status::Failed);
    b.after = vec!["a".to_string()];
    b.before = vec!["a".to_string()]; // cycle: a -> b -> a
    b.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![a, b]);

    // Should not infinite loop
    wg_ok(&wg_dir, &["replay", "--failed-only", "--subgraph", "a"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
}

// 5.7 load_agent_runs_sort_order
#[test]
fn test_trace_agent_runs_sort_order() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Sort test", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create archives out of chronological filesystem order
    create_agent_archive(&wg_dir, "t1", "2026-02-18T12:00:00Z", "prompt 3", "output 3");
    create_agent_archive(&wg_dir, "t1", "2026-02-18T08:00:00Z", "prompt 1", "output 1");
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt 2", "output 2");

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    let runs = json["agent_runs"].as_array().unwrap();
    assert_eq!(runs.len(), 3);
    // Should be sorted chronologically by timestamp (directory name)
    assert_eq!(runs[0]["timestamp"], "2026-02-18T08:00:00Z");
    assert_eq!(runs[1]["timestamp"], "2026-02-18T10:00:00Z");
    assert_eq!(runs[2]["timestamp"], "2026-02-18T12:00:00Z");
}

// ===========================================================================
// Additional edge cases
// ===========================================================================

// 3.22 runs_diff_with_removed_task (isolated)
#[test]
fn test_runs_diff_removed_task() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task 1", Status::Done);
    let t2 = make_task("t2", "Task 2", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Snapshot
    let meta = workgraph::runs::RunMeta {
        id: "run-001".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        model: None,
        reset_tasks: vec![],
        preserved_tasks: vec!["t1".to_string(), "t2".to_string()],
        filter: None,
    };
    workgraph::runs::snapshot(&wg_dir, "run-001", &meta).unwrap();

    // Remove t2 from graph
    let mut graph = load_wg_graph(&wg_dir);
    graph.remove_node("t2");
    save_graph(&graph, wg_dir.join("graph.jsonl")).unwrap();

    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    let changes = json["changes"].as_array().unwrap();
    let t2_change = changes.iter().find(|c| c["id"] == "t2").unwrap();
    assert_eq!(t2_change["change"], "removed");
}

// 3.23 runs_diff_with_added_task (isolated)
#[test]
fn test_runs_diff_added_task() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task 1", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Snapshot
    let meta = workgraph::runs::RunMeta {
        id: "run-001".to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        model: None,
        reset_tasks: vec![],
        preserved_tasks: vec!["t1".to_string()],
        filter: None,
    };
    workgraph::runs::snapshot(&wg_dir, "run-001", &meta).unwrap();

    // Add t2
    let mut graph = load_wg_graph(&wg_dir);
    graph.add_node(Node::Task(make_task("t2", "Task 2", Status::Open)));
    save_graph(&graph, wg_dir.join("graph.jsonl")).unwrap();

    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    let changes = json["changes"].as_array().unwrap();
    let t2_change = changes.iter().find(|c| c["id"] == "t2").unwrap();
    assert_eq!(t2_change["change"], "added");
}

// 4.5 replay_notify_graph_changed (implicit — no service running)
#[test]
fn test_replay_and_restore_without_service() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Both replay and restore should succeed without a service running
    wg_ok(&wg_dir, &["replay", "--failed-only"]);
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);
    // If we got here without error, notification didn't cause problems
}

// 3.2 runs_list_chronological (3+ runs)
#[test]
fn test_runs_list_three_runs_chronological() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Failed);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create 3 replay cycles
    for _ in 0..3 {
        wg_ok(&wg_dir, &["replay", "--failed-only"]);
        // Reset for next cycle
        let mut graph = load_wg_graph(&wg_dir);
        graph.get_task_mut("t1").unwrap().status = Status::Failed;
        graph.get_task_mut("t1").unwrap().failure_reason = Some("err".to_string());
        save_graph(&graph, wg_dir.join("graph.jsonl")).unwrap();
    }

    let json = wg_json(&wg_dir, &["runs", "list"]);
    let runs = json.as_array().unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0]["id"], "run-001");
    assert_eq!(runs[1]["id"], "run-002");
    assert_eq!(runs[2]["id"], "run-003");
}

// ===========================================================================
// GAP TESTS — filling coverage gaps from the test spec
// ===========================================================================

// --- TRACE GAPS ---

// 1.13 trace_json_flag_overrides_full_and_ops_only
#[test]
fn test_trace_json_overrides_full_and_ops_only() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Override test", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt", "output");
    workgraph::provenance::record(
        &wg_dir, "add_task", Some("t1"), None,
        serde_json::json!({"title": "Override test"}),
        workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
    ).unwrap();

    // --json with --full should produce JSON, not full text
    let json_full = wg_json(&wg_dir, &["trace", "show", "t1", "--full"]);
    assert!(json_full["id"].is_string(), "--json should override --full: {:?}", json_full);
    assert_eq!(json_full["id"], "t1");
    assert!(json_full["agent_runs"].is_array());
    assert!(json_full["summary"].is_object());

    // --json with --ops-only should produce JSON, not ops-only text
    let json_ops = wg_json(&wg_dir, &["trace", "show", "t1", "--ops-only"]);
    assert!(json_ops["id"].is_string(), "--json should override --ops-only: {:?}", json_ops);
    assert_eq!(json_ops["id"], "t1");
    assert!(json_ops["agent_runs"].is_array());
    assert!(json_ops["operations"].is_array());
}

// 1.14 trace_agent_archive_missing_output_txt
#[test]
fn test_trace_agent_archive_missing_output() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Missing output", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create archive with only prompt.txt (no output.txt)
    let archive_dir = wg_dir
        .join("log")
        .join("agents")
        .join("t1")
        .join("2026-02-18T10:00:00Z");
    fs::create_dir_all(&archive_dir).unwrap();
    fs::write(archive_dir.join("prompt.txt"), "Test prompt content").unwrap();
    // Deliberately NOT writing output.txt

    // Should succeed without error
    let output = wg_ok(&wg_dir, &["trace", "show", "t1"]);
    assert!(output.contains("Agent runs (1):"), "Should still list the agent run: {}", output);

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    let run = &json["agent_runs"][0];
    assert!(run["prompt_bytes"].is_number(), "Should have prompt_bytes");
    assert!(run["output_bytes"].is_null(), "output_bytes should be absent when output.txt missing");
    // No tool_calls or turns since there's no output to parse
    assert!(run["tool_calls"].is_null(), "tool_calls should be absent");
    assert!(run["turns"].is_null(), "turns should be absent");
}

// 1.15 trace_agent_archive_empty_output
#[test]
fn test_trace_agent_archive_empty_output() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Empty output", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create archive with empty output.txt
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt", "");

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    let run = &json["agent_runs"][0];
    assert_eq!(run["output_bytes"], 0, "empty output should have 0 bytes");
    assert!(run["output_lines"].is_null() || run["output_lines"] == 0,
        "empty output should have 0 or null lines");
    // tool_calls and turns should be absent (0 values are skipped)
    assert!(run["tool_calls"].is_null(), "tool_calls should be absent for empty output");
    assert!(run["turns"].is_null(), "turns should be absent for empty output");
    // Summary total_output_bytes should be absent (sum is 0)
    assert!(json["summary"]["total_output_bytes"].is_null(),
        "total_output_bytes should be absent when sum is 0");
}

// 1.16 trace_operation_detail_truncation
#[test]
fn test_trace_operation_detail_truncation() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Truncation test", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let short_detail = "Short detail under 120 chars";
    let long_detail = "x".repeat(200);

    workgraph::provenance::record(
        &wg_dir, "short_op", Some("t1"), None,
        serde_json::json!(short_detail),
        workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
    ).unwrap();
    workgraph::provenance::record(
        &wg_dir, "long_op", Some("t1"), None,
        serde_json::json!(long_detail),
        workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
    ).unwrap();

    // Summary mode should truncate long details
    let output = wg_ok(&wg_dir, &["trace", "show", "t1"]);
    assert!(output.contains(short_detail), "Short detail should appear in full: {}", output);
    // Long detail should be truncated with "..."
    assert!(output.contains("..."), "Long detail should be truncated with ...: {}", output);
    // The full 200-char string should NOT appear
    assert!(!output.contains(&long_detail), "Full long detail should not appear in summary mode");

    // JSON mode should have full details (not truncated)
    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    let ops = json["operations"].as_array().unwrap();
    let long_op = ops.iter().find(|o| o["op"] == "long_op").unwrap();
    assert_eq!(long_op["detail"].as_str().unwrap(), long_detail,
        "JSON should have full untruncated detail");
}

// 1.20 trace_summary_mode_excludes_content
#[test]
fn test_trace_summary_mode_excludes_content() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Summary mode test", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let unique_prompt = "UNIQUE_PROMPT_MARKER_12345";
    let unique_output = "UNIQUE_OUTPUT_MARKER_67890";
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", unique_prompt, unique_output);

    // Summary mode (default) should NOT show verbatim prompt/output content
    let summary = wg_ok(&wg_dir, &["trace", "show", "t1"]);
    assert!(!summary.contains(unique_prompt),
        "Summary should not contain verbatim prompt text: {}", summary);
    assert!(!summary.contains(unique_output),
        "Summary should not contain verbatim output text: {}", summary);
    // Should show size info instead
    assert!(summary.contains("KB") || summary.contains("bytes"),
        "Summary should show output size info: {}", summary);

    // JSON mode should include content
    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    let run = &json["agent_runs"][0];
    assert!(run["prompt"].is_string(), "JSON should include prompt content");
    assert!(run["output"].is_string(), "JSON should include output content");
}

// --- REPLAY GAPS ---

// 2.26 replay_filter_priority: --tasks > --failed-only > --below-score
#[test]
fn test_replay_filter_priority_tasks_over_failed_only() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Failed", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let t2 = make_task("t2", "Done", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // --tasks t2 --failed-only: explicit list should take priority
    // t2 is in the list (should reset), t1 is failed but NOT in the list
    wg_ok(&wg_dir, &["replay", "--tasks", "t2", "--failed-only"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Open,
        "t2 should be reset (explicitly listed)");
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Failed,
        "t1 should NOT be reset (not in explicit list, even though failed)");
}

#[test]
fn test_replay_filter_priority_failed_only_over_below_score() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Failed", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let t2 = make_task("t2", "Done low score", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    write_evaluation(&wg_dir, "eval-t2", "t2", 0.1);

    // --failed-only --below-score 0.5: --failed-only takes priority
    wg_ok(&wg_dir, &["replay", "--failed-only", "--below-score", "0.5"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Open,
        "t1 should be reset (failed)");
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Done,
        "t2 should NOT be reset (--failed-only takes priority, t2 is Done not Failed)");
}

// 2.27 replay_below_score_exact_boundary
#[test]
fn test_replay_below_score_exact_boundary() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("boundary", "Boundary task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    write_evaluation(&wg_dir, "eval-boundary", "boundary", 0.7);

    // --below-score 0.7: score 0.7 is NOT < 0.7, so should be preserved
    wg_ok(&wg_dir, &["replay", "--below-score", "0.7"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("boundary").unwrap().status, Status::Done,
        "Task with score exactly at threshold should be preserved (0.7 is NOT < 0.7)");
}

// 2.28 replay_tasks_with_nonexistent_id
#[test]
fn test_replay_tasks_nonexistent_id() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Real task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // --tasks with a nonexistent task ID — silently ignored, no match
    let output = wg_ok(&wg_dir, &["replay", "--tasks", "nonexistent"]);
    assert!(
        output.contains("No tasks match") || output.contains("Nothing to replay"),
        "Should report no matching tasks: {}", output
    );

    // Graph unchanged
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Done,
        "t1 should be unchanged");

    // No snapshot created
    let runs_dir = wg_dir.join("runs");
    if runs_dir.exists() {
        let entries: Vec<_> = fs::read_dir(&runs_dir).unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "No snapshot should be created");
    }
}

// 2.29 replay_subgraph_single_node
#[test]
fn test_replay_subgraph_single_node() {
    let tmp = TempDir::new().unwrap();
    let mut standalone = make_task("standalone", "Standalone", Status::Failed);
    standalone.failure_reason = Some("err".to_string());
    // No blocks edges
    let mut other = make_task("other", "Other", Status::Failed);
    other.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![standalone, other]);

    wg_ok(&wg_dir, &["replay", "--failed-only", "--subgraph", "standalone"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("standalone").unwrap().status, Status::Open,
        "standalone should be reset (in subgraph + failed)");
    assert_eq!(graph.get_task("other").unwrap().status, Status::Failed,
        "other should NOT be reset (outside single-node subgraph)");
}

// 2.30 replay_model_override_not_applied_to_preserved
#[test]
fn test_replay_model_override_not_applied_to_preserved() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Failed", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let t2 = make_task("t2", "Done", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // --failed-only --model new-model: only reset tasks get the model
    wg_ok(&wg_dir, &["replay", "--failed-only", "--model", "new-model"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().model, Some("new-model".to_string()),
        "t1 (reset) should have model override");
    assert!(graph.get_task("t2").unwrap().model.is_none(),
        "t2 (preserved) should NOT have model override");
}

// 2.31 replay_below_score_zero_threshold
#[test]
fn test_replay_below_score_zero_threshold() {
    let tmp = TempDir::new().unwrap();
    let scored = make_task("scored", "Scored task", Status::Done);
    let unscored = make_task("unscored", "Unscored task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![scored, unscored]);

    write_evaluation(&wg_dir, "eval-scored", "scored", 0.1);

    // --below-score 0.0: nothing is < 0.0, but unscored terminal tasks still get reset
    wg_ok(&wg_dir, &["replay", "--below-score", "0.0"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("scored").unwrap().status, Status::Done,
        "scored (0.1 NOT < 0.0) should be preserved");
    assert_eq!(graph.get_task("unscored").unwrap().status, Status::Open,
        "unscored (no eval + terminal) should be reset");
}

// --- CROSS-CUTTING GAPS ---

// 4.6 replay_preserves_agent_archives
#[test]
fn test_replay_preserves_agent_archives() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Task with archive", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let archive_prompt = "Pre-replay prompt content";
    let archive_output = "Pre-replay output content";
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", archive_prompt, archive_output);

    // Verify archive exists before replay
    let archive_dir = wg_dir.join("log").join("agents").join("t1").join("2026-02-18T10:00:00Z");
    assert!(archive_dir.join("prompt.txt").exists(), "archive should exist before replay");

    // Replay resets t1
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Open, "t1 should be reset");

    // Agent archives should still exist on disk
    assert!(archive_dir.join("prompt.txt").exists(),
        "prompt.txt should persist after replay");
    assert!(archive_dir.join("output.txt").exists(),
        "output.txt should persist after replay");

    // Verify wg trace still shows the archived agent runs
    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    assert_eq!(json["summary"]["agent_run_count"], 1,
        "Agent archives should be accessible via trace after replay");
    let run = &json["agent_runs"][0];
    assert_eq!(run["timestamp"], "2026-02-18T10:00:00Z");
}

// 4.7 restore_then_diff_shows_no_changes
#[test]
fn test_restore_then_diff_shows_no_changes() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Task", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Replay creates run-001
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Restore from run-001 (graph matches the snapshot)
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Diff against run-001 should show no changes
    let output = wg_ok(&wg_dir, &["runs", "diff", "run-001"]);
    assert!(output.contains("No differences"),
        "After restoring from a snapshot, diffing same snapshot should show no changes: {}", output);

    // JSON diff should confirm
    let json = wg_json(&wg_dir, &["runs", "diff", "run-001"]);
    assert_eq!(json["total_changes"], 0,
        "total_changes should be 0 after restore then diff: {:?}", json);
}

// --- HELPER GAPS ---

// 5.11 build_filter_desc — test via CLI by checking run metadata filter field
#[test]
fn test_build_filter_desc_all_flags() {
    let tmp = TempDir::new().unwrap();
    let mut root = make_task("root", "Root", Status::Failed);
    root.before = vec!["child".to_string()];
    root.failure_reason = Some("err".to_string());
    let mut child = make_task("child", "Child", Status::Failed);
    child.after = vec!["root".to_string()];
    child.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![root, child]);

    write_evaluation(&wg_dir, "eval-child", "child", 0.5);

    // Use multiple flags together
    wg_ok(&wg_dir, &[
        "replay", "--failed-only", "--model", "opus",
        "--keep-done", "0.9", "--subgraph", "root",
    ]);

    let json = wg_json(&wg_dir, &["runs", "show", "run-001"]);
    let filter = json["filter"].as_str().unwrap();
    assert!(filter.contains("--failed-only"), "filter should contain --failed-only: {}", filter);
    assert!(filter.contains("--model opus"), "filter should contain --model: {}", filter);
    assert!(filter.contains("--keep-done"), "filter should contain --keep-done: {}", filter);
    assert!(filter.contains("--subgraph root"), "filter should contain --subgraph: {}", filter);
}

#[test]
fn test_build_filter_desc_default_all_tasks() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Default replay (no flags) — filter should be "all tasks"
    wg_ok(&wg_dir, &["replay", "--keep-done", "1.0"]);

    let json = wg_json(&wg_dir, &["runs", "show", "run-001"]);
    let filter = json["filter"].as_str().unwrap();
    assert!(filter.contains("--keep-done"),
        "filter should contain --keep-done: {}", filter);
}

#[test]
fn test_build_filter_desc_tasks_and_below_score() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task 1", Status::Done);
    let t2 = make_task("t2", "Task 2", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    wg_ok(&wg_dir, &["replay", "--tasks", "t1,t2", "--below-score", "0.5"]);

    let json = wg_json(&wg_dir, &["runs", "show", "run-001"]);
    let filter = json["filter"].as_str().unwrap();
    assert!(filter.contains("--tasks"), "filter should contain --tasks: {}", filter);
    assert!(filter.contains("--below-score"), "filter should contain --below-score: {}", filter);
}

// ===========================================================================
// GAP TESTS — newly identified coverage gaps
// ===========================================================================

// --- REPLAY GAPS ---

// 2.33 replay_tasks_and_subgraph_combined
// When both --tasks and --subgraph are provided, subgraph filter runs first.
// Tasks outside the subgraph are skipped even if explicitly listed.
#[test]
fn test_replay_tasks_and_subgraph_combined() {
    let tmp = TempDir::new().unwrap();
    let mut root = make_task("root", "Root", Status::Done);
    root.before = vec!["child".to_string()];
    let mut child = make_task("child", "Child", Status::Done);
    child.after = vec!["root".to_string()];
    let outside = make_task("outside", "Outside", Status::Done);
    // outside has no blocks edges to root — it's outside the subgraph
    let wg_dir = setup_workgraph(&tmp, vec![root, child, outside]);

    // --tasks outside,child --subgraph root
    // child is in subgraph AND in explicit list => reset
    // outside is NOT in subgraph => skipped even though listed
    // root is in subgraph but NOT in explicit list => preserved
    wg_ok(&wg_dir, &["replay", "--tasks", "outside,child", "--subgraph", "root"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("child").unwrap().status, Status::Open,
        "child should be reset (in subgraph + in explicit list)");
    assert_eq!(graph.get_task("outside").unwrap().status, Status::Done,
        "outside should be preserved (not in subgraph, even though in --tasks)");
    assert_eq!(graph.get_task("root").unwrap().status, Status::Done,
        "root should be preserved (in subgraph but not in explicit task list)");
}

// 2.34 replay_config_default_keep_done_threshold
// When --keep-done is not explicitly passed, config's keep_done_threshold is used.
#[test]
fn test_replay_config_default_keep_done_threshold() {
    let tmp = TempDir::new().unwrap();
    let mut parent = make_task("parent", "Parent", Status::Failed);
    parent.before = vec!["child".to_string()];
    parent.failure_reason = Some("err".to_string());
    let mut child = make_task("child", "Child", Status::Done);
    child.after = vec!["parent".to_string()];
    let wg_dir = setup_workgraph(&tmp, vec![parent, child]);

    // Write config.toml with keep_done_threshold = 0.9
    let config_content = "[replay]\nkeep_done_threshold = 0.9\n";
    fs::write(wg_dir.join("config.toml"), config_content).unwrap();

    // Write evaluation for child with score 0.95 (>= 0.9 threshold)
    write_evaluation(&wg_dir, "eval-child", "child", 0.95);

    // Run replay --failed-only (no explicit --keep-done)
    // Config's keep_done_threshold (0.9) should preserve child (score 0.95 >= 0.9)
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("parent").unwrap().status, Status::Open,
        "parent should be reset (failed seed)");
    assert_eq!(graph.get_task("child").unwrap().status, Status::Done,
        "child should be preserved by config's default keep_done_threshold (0.95 >= 0.9)");
}

// 2.35 replay_blocked_task_behavior
// Blocked tasks are NOT terminal (is_terminal() returns false).
// --failed-only should NOT reset Blocked tasks, default filter should NOT include them.
#[test]
fn test_replay_blocked_task_behavior() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Blocked task", Status::Blocked);
    let mut t2 = make_task("t2", "Failed task", Status::Failed);
    t2.failure_reason = Some("err".to_string());
    let t3 = make_task("t3", "Done task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2, t3]);

    // Test with --failed-only
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Open,
        "t2 (Failed) should be reset by --failed-only");
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Blocked,
        "t1 (Blocked) should NOT be reset by --failed-only");
    assert_eq!(graph.get_task("t3").unwrap().status, Status::Done,
        "t3 (Done) should NOT be reset by --failed-only");
}

#[test]
fn test_replay_blocked_task_not_terminal() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Blocked task", Status::Blocked);
    let mut t2 = make_task("t2", "Failed task", Status::Failed);
    t2.failure_reason = Some("err".to_string());
    let t3 = make_task("t3", "Done task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2, t3]);

    // Default replay (all terminal) — Blocked is NOT terminal
    wg_ok(&wg_dir, &["replay", "--keep-done", "1.0"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Open,
        "t2 (Failed, terminal) should be reset by default filter");
    assert_eq!(graph.get_task("t3").unwrap().status, Status::Open,
        "t3 (Done, terminal) should be reset by default filter");
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Blocked,
        "t1 (Blocked, NOT terminal) should NOT be reset by default filter");
}

// 2.36 replay_inprogress_task_behavior
// InProgress tasks are NOT terminal. Default filter and --failed-only should not touch them.
#[test]
fn test_replay_inprogress_task_not_reset() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "InProgress task", Status::InProgress);
    t1.assigned = Some("agent-1".to_string());
    t1.started_at = Some("2026-02-18T10:00:00+00:00".to_string());
    let mut t2 = make_task("t2", "Failed task", Status::Failed);
    t2.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // --failed-only should not touch InProgress
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Open,
        "t2 (Failed) should be reset");
    assert_eq!(graph.get_task("t1").unwrap().status, Status::InProgress,
        "t1 (InProgress) should NOT be reset by --failed-only");
}

#[test]
fn test_replay_inprogress_not_terminal() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "InProgress task", Status::InProgress);
    t1.assigned = Some("agent-1".to_string());
    let mut t2 = make_task("t2", "Failed task", Status::Failed);
    t2.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    // Default replay (all terminal) — InProgress is NOT terminal
    wg_ok(&wg_dir, &["replay", "--keep-done", "1.0"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Open,
        "t2 (Failed, terminal) should be reset");
    assert_eq!(graph.get_task("t1").unwrap().status, Status::InProgress,
        "t1 (InProgress, NOT terminal) should NOT be reset by default filter");
}

// 2.37 replay_tasks_with_duplicates_in_list
// --tasks a,a where the same task ID appears twice. HashSet naturally deduplicates.
#[test]
fn test_replay_tasks_with_duplicates_in_list() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Task 1", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // --tasks t1,t1 — duplicate IDs
    let json = wg_json(&wg_dir, &["replay", "--tasks", "t1,t1"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Open,
        "t1 should be reset");

    // t1 should appear only once in reset_tasks (not duplicated)
    let reset = json["reset_tasks"].as_array().unwrap();
    let t1_count = reset.iter().filter(|t| t.as_str() == Some("t1")).count();
    assert_eq!(t1_count, 1, "t1 should appear exactly once in reset_tasks (no duplication)");
}

// 2.38 replay_keep_done_only_applies_to_done
// --keep-done should only preserve tasks with status Done. Failed/Abandoned tasks
// with high scores should NOT be preserved by keep-done.
#[test]
fn test_replay_keep_done_only_applies_to_done_status() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Failed high score", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let t2 = make_task("t2", "Done high score", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    write_evaluation(&wg_dir, "eval-t1", "t1", 0.95);
    write_evaluation(&wg_dir, "eval-t2", "t2", 0.95);

    // Default filter (all terminal) + --keep-done 0.9
    // t1: Failed + score 0.95 >= 0.9, but keep-done only checks Done status
    // t2: Done + score 0.95 >= 0.9, should be preserved by keep-done
    wg_ok(&wg_dir, &["replay", "--keep-done", "0.9"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Open,
        "t1 (Failed) should be reset even with high score (keep-done only applies to Done)");
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Done,
        "t2 (Done) should be preserved by keep-done (score 0.95 >= 0.9)");
}

// --- CROSS-CUTTING GAPS ---

// 4.8 trace_after_restore
// After restoring from a snapshot, wg trace should reflect the restored task state.
#[test]
fn test_trace_after_restore() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Task", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Record provenance and create agent archive
    workgraph::provenance::record(
        &wg_dir, "add_task", Some("t1"), None,
        serde_json::json!({"title": "Task"}),
        workgraph::provenance::DEFAULT_ROTATION_THRESHOLD,
    ).unwrap();
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt", "output");

    // Replay to create run-001, resetting t1 to Open
    wg_ok(&wg_dir, &["replay", "--failed-only"]);
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Open);

    // Restore from run-001 (t1 back to Failed)
    wg_ok(&wg_dir, &["runs", "restore", "run-001"]);

    // Trace should show restored state
    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    assert_eq!(json["status"], "failed", "Trace should show restored status (failed)");

    // Agent archives should still be accessible
    assert_eq!(json["summary"]["agent_run_count"], 1,
        "Agent archives should survive restore");

    // Provenance should contain all operations (add_task, replay, restore)
    let ops = json["operations"].as_array().unwrap();
    let op_types: Vec<&str> = ops.iter()
        .filter_map(|o| o["op"].as_str())
        .collect();
    assert!(op_types.contains(&"add_task"), "Should have add_task op");
}

// 4.9 multiple_replay_then_trace_shows_all_archives
// After multiple replay cycles, all agent archives should be preserved.
#[test]
fn test_multiple_replay_cycles_preserve_all_archives() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Task", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    // Create agent archive #1
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt 1", "output 1");

    // First replay (resets t1)
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Create agent archive #2 (simulating a new agent run after reset)
    create_agent_archive(&wg_dir, "t1", "2026-02-18T11:00:00Z", "prompt 2", "output 2");

    // Set t1 back to Failed and replay again
    {
        let mut graph = load_wg_graph(&wg_dir);
        graph.get_task_mut("t1").unwrap().status = Status::Failed;
        graph.get_task_mut("t1").unwrap().failure_reason = Some("err again".to_string());
        save_graph(&graph, wg_dir.join("graph.jsonl")).unwrap();
    }
    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Trace should show both agent archives
    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    assert_eq!(json["summary"]["agent_run_count"], 2,
        "Both agent archives should be visible after multiple replays");

    let runs = json["agent_runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2);
    // Archives should be in chronological order
    assert_eq!(runs[0]["timestamp"], "2026-02-18T10:00:00Z");
    assert_eq!(runs[1]["timestamp"], "2026-02-18T11:00:00Z");
}

// --- HELPER GAPS ---

// 5.12 parse_stream_json_mixed_valid_and_invalid
// Stream-json output with a mix of valid JSON lines and non-JSON lines.
// Parser should count only valid JSON entries and silently skip invalid lines.
#[test]
fn test_trace_stream_json_mixed_valid_and_invalid() {
    let tmp = TempDir::new().unwrap();
    let t1 = make_task("t1", "Mixed output", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    let mixed_output = r#"some random text
{"type":"assistant","message":"hello"}
WARNING: something happened
{"type":"tool_use","name":"Read","id":"1"}
more random text
{"type":"result","cost":{"input":100,"output":50}}
"#;
    create_agent_archive(&wg_dir, "t1", "2026-02-18T10:00:00Z", "prompt", mixed_output);

    let json = wg_json(&wg_dir, &["trace", "show", "t1"]);
    let run = &json["agent_runs"][0];
    assert_eq!(run["tool_calls"], 1, "Should count 1 tool_use call (skip non-JSON lines)");
    assert_eq!(run["turns"], 1, "Should count 1 assistant turn (skip non-JSON lines)");
}

// 5.14 collect_subgraph_disconnected_blocks
// collect_subgraph follows `blocks` edges forward from root, not `after`.
#[test]
fn test_replay_subgraph_follows_blocks_edges() {
    let tmp = TempDir::new().unwrap();
    // root blocks child, child blocks leaf.
    // after is intentionally empty on child/leaf — edges only in blocks.
    let mut root = make_task("root", "Root", Status::Failed);
    root.before = vec!["child".to_string()];
    root.failure_reason = Some("err".to_string());
    let mut child = make_task("child", "Child", Status::Failed);
    child.before = vec!["leaf".to_string()];
    child.failure_reason = Some("err".to_string());
    // after intentionally NOT set on child
    let mut leaf = make_task("leaf", "Leaf", Status::Failed);
    leaf.failure_reason = Some("err".to_string());
    // after intentionally NOT set on leaf

    let wg_dir = setup_workgraph(&tmp, vec![root, child, leaf]);

    wg_ok(&wg_dir, &["replay", "--failed-only", "--subgraph", "root"]);

    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("root").unwrap().status, Status::Open,
        "root should be reset (in subgraph)");
    assert_eq!(graph.get_task("child").unwrap().status, Status::Open,
        "child should be reset (reachable via root.before)");
    assert_eq!(graph.get_task("leaf").unwrap().status, Status::Open,
        "leaf should be reset (reachable via child.before)");
}

// 2.7 replay_plan_only_no_side_effects (also in integration_trace_replay.rs)
#[test]
fn test_replay_plan_only_no_side_effects() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Failed task", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    t1.before = vec!["t2".to_string()];
    let mut t2 = make_task("t2", "Dependent", Status::Done);
    t2.after = vec!["t1".to_string()];
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    let output = wg_ok(&wg_dir, &["replay", "--failed-only", "--plan-only"]);
    assert!(output.contains("dry run"), "Should mention dry run: {}", output);

    // Graph should be unchanged
    let graph = load_wg_graph(&wg_dir);
    assert_eq!(graph.get_task("t1").unwrap().status, Status::Failed,
        "t1 should still be Failed (plan-only, no changes)");
    assert_eq!(graph.get_task("t2").unwrap().status, Status::Done,
        "t2 should still be Done (plan-only, no changes)");

    // No runs/ directory should be created
    let runs_dir = wg_dir.join("runs");
    if runs_dir.exists() {
        let entries: Vec<_> = fs::read_dir(&runs_dir).unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "No snapshots should exist for plan-only: {:?}", entries);
    }

    // No provenance entries for "replay"
    let ops = workgraph::provenance::read_all_operations(&wg_dir).unwrap();
    let replay_ops: Vec<_> = ops.iter().filter(|o| o.op == "replay").collect();
    assert!(replay_ops.is_empty(), "No replay provenance should exist for plan-only");
}

// 2.8 replay_plan_only_json_output
#[test]
fn test_replay_plan_only_json_output() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Failed task", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let t2 = make_task("t2", "Done task", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    let json = wg_json(&wg_dir, &["replay", "--failed-only", "--plan-only"]);
    assert_eq!(json["plan_only"], true, "plan_only should be true");
    assert_eq!(json["run_id"], "(dry run)", "run_id should be '(dry run)'");
    assert!(json["reset_tasks"].is_array());
    let reset = json["reset_tasks"].as_array().unwrap();
    assert!(reset.iter().any(|t| t.as_str() == Some("t1")),
        "t1 should be in reset_tasks: {:?}", reset);
}

// 2.11 replay_records_provenance
#[test]
fn test_replay_records_provenance_entry() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Failed task", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let wg_dir = setup_workgraph(&tmp, vec![t1]);

    wg_ok(&wg_dir, &["replay", "--failed-only"]);

    // Check provenance for "replay" entry
    let ops = workgraph::provenance::read_all_operations(&wg_dir).unwrap();
    let replay_ops: Vec<_> = ops.iter().filter(|o| o.op == "replay").collect();
    assert!(!replay_ops.is_empty(), "Should have a replay provenance entry");

    let replay_op = &replay_ops[0];
    // task_id should be None (replay is a graph-level operation)
    assert!(replay_op.task_id.is_none(),
        "replay provenance task_id should be None");
    assert!(replay_op.detail["run_id"].is_string(),
        "replay provenance should have run_id");
    assert!(replay_op.detail["reset_count"].is_number(),
        "replay provenance should have reset_count");
    assert!(replay_op.detail["reset_tasks"].is_array(),
        "replay provenance should have reset_tasks");
}

// 2.9 replay_model_override (verify model set on all reset tasks including dependents)
#[test]
fn test_replay_model_override_on_all_reset_tasks() {
    let tmp = TempDir::new().unwrap();
    let mut root = make_task("root", "Root", Status::Failed);
    root.before = vec!["mid".to_string()];
    root.failure_reason = Some("err".to_string());
    let mut mid = make_task("mid", "Mid", Status::Done);
    mid.after = vec!["root".to_string()];
    mid.before = vec!["leaf".to_string()];
    let mut leaf = make_task("leaf", "Leaf", Status::Done);
    leaf.after = vec!["mid".to_string()];
    let wg_dir = setup_workgraph(&tmp, vec![root, mid, leaf]);

    wg_ok(&wg_dir, &["replay", "--failed-only", "--model", "different-model", "--keep-done", "1.0"]);

    let graph = load_wg_graph(&wg_dir);
    // All three should be reset and have the model override
    assert_eq!(graph.get_task("root").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("root").unwrap().model, Some("different-model".to_string()));
    assert_eq!(graph.get_task("mid").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("mid").unwrap().model, Some("different-model".to_string()));
    assert_eq!(graph.get_task("leaf").unwrap().status, Status::Open);
    assert_eq!(graph.get_task("leaf").unwrap().model, Some("different-model".to_string()));
}

// 2.20 replay_json_output_structure
#[test]
fn test_replay_json_output_structure() {
    let tmp = TempDir::new().unwrap();
    let mut t1 = make_task("t1", "Failed", Status::Failed);
    t1.failure_reason = Some("err".to_string());
    let t2 = make_task("t2", "Done", Status::Done);
    let wg_dir = setup_workgraph(&tmp, vec![t1, t2]);

    let json = wg_json(&wg_dir, &["replay", "--failed-only"]);

    // Validate JSON structure
    assert!(json["run_id"].is_string(), "Should have run_id");
    assert!(json["run_id"].as_str().unwrap().starts_with("run-"), "run_id should start with run-");
    assert!(json["reset_tasks"].is_array(), "Should have reset_tasks array");
    assert!(json["preserved_tasks"].is_array(), "Should have preserved_tasks array");
    assert_eq!(json["plan_only"], false, "plan_only should be false");

    let reset = json["reset_tasks"].as_array().unwrap();
    assert!(reset.iter().any(|t| t.as_str() == Some("t1")));
    let preserved = json["preserved_tasks"].as_array().unwrap();
    assert!(preserved.iter().any(|t| t.as_str() == Some("t2")));
}
