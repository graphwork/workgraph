//! Integration tests for the provenance/logging system.
//!
//! Verifies that every graph-mutating command records an operation log entry,
//! that agent conversation archives work correctly, that log rotation with
//! zstd compression is reliable, and that operation log + graph state remain
//! coherent.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};
use workgraph::provenance::{self, OperationEntry};

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

fn setup_wg(tasks: Vec<Task>) -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &graph_path).unwrap();
    (dir, wg_dir)
}

fn ops_with_op<'a>(entries: &'a [OperationEntry], op: &str) -> Vec<&'a OperationEntry> {
    entries.iter().filter(|e| e.op == op).collect()
}

// ── Goal 1: Every graph-mutating command produces an operation log entry ──

#[test]
fn add_produces_operation_log_entry() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

    wg_ok(&wg_dir, &["add", "My test task", "--id", "test-task"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let add_ops = ops_with_op(&entries, "add_task");
    assert_eq!(add_ops.len(), 1, "Expected 1 add_task op, got {}", add_ops.len());
    assert_eq!(add_ops[0].task_id.as_deref(), Some("test-task"));
}

#[test]
fn edit_produces_operation_log_entry() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

    wg_ok(&wg_dir, &["add", "Edit me", "--id", "edit-task"]);
    wg_ok(&wg_dir, &["edit", "edit-task", "--title", "Edited title"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let edit_ops = ops_with_op(&entries, "edit");
    assert_eq!(edit_ops.len(), 1, "Expected 1 edit op, got {}", edit_ops.len());
    assert_eq!(edit_ops[0].task_id.as_deref(), Some("edit-task"));
}

#[test]
fn done_produces_operation_log_entry() {
    let (_dir, wg_dir) = setup_wg(vec![make_task("t1", "Task 1", Status::Open)]);

    wg_ok(&wg_dir, &["done", "t1"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let done_ops = ops_with_op(&entries, "done");
    assert_eq!(done_ops.len(), 1);
    assert_eq!(done_ops[0].task_id.as_deref(), Some("t1"));
}

#[test]
fn fail_produces_operation_log_entry() {
    let mut task = make_task("t1", "Task 1", Status::InProgress);
    task.assigned = Some("agent-1".to_string());
    let (_dir, wg_dir) = setup_wg(vec![task]);

    wg_ok(&wg_dir, &["fail", "t1", "--reason", "test failure"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let fail_ops = ops_with_op(&entries, "fail");
    assert_eq!(fail_ops.len(), 1);
    assert_eq!(fail_ops[0].task_id.as_deref(), Some("t1"));
    // Check detail contains reason
    assert!(fail_ops[0].detail.get("reason").is_some());
}

#[test]
fn abandon_produces_operation_log_entry() {
    let (_dir, wg_dir) = setup_wg(vec![make_task("t1", "Task 1", Status::Open)]);

    wg_ok(&wg_dir, &["abandon", "t1", "--reason", "not needed"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let abandon_ops = ops_with_op(&entries, "abandon");
    assert_eq!(abandon_ops.len(), 1);
    assert_eq!(abandon_ops[0].task_id.as_deref(), Some("t1"));
    assert!(abandon_ops[0].detail.get("reason").is_some());
}

#[test]
fn retry_produces_operation_log_entry() {
    let mut task = make_task("t1", "Task 1", Status::Failed);
    task.retry_count = 1;
    let (_dir, wg_dir) = setup_wg(vec![task]);

    wg_ok(&wg_dir, &["retry", "t1"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let retry_ops = ops_with_op(&entries, "retry");
    assert_eq!(retry_ops.len(), 1);
    assert_eq!(retry_ops[0].task_id.as_deref(), Some("t1"));
    assert!(retry_ops[0].detail.get("attempt").is_some());
}

#[test]
fn claim_produces_operation_log_entry() {
    let (_dir, wg_dir) = setup_wg(vec![make_task("t1", "Task 1", Status::Open)]);

    wg_ok(&wg_dir, &["claim", "t1", "--actor", "agent-1"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let claim_ops = ops_with_op(&entries, "claim");
    assert_eq!(claim_ops.len(), 1);
    assert_eq!(claim_ops[0].task_id.as_deref(), Some("t1"));
    assert_eq!(claim_ops[0].actor.as_deref(), Some("agent-1"));
}

#[test]
fn unclaim_produces_operation_log_entry() {
    let mut task = make_task("t1", "Task 1", Status::InProgress);
    task.assigned = Some("agent-1".to_string());
    let (_dir, wg_dir) = setup_wg(vec![task]);

    wg_ok(&wg_dir, &["unclaim", "t1"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let unclaim_ops = ops_with_op(&entries, "unclaim");
    assert_eq!(unclaim_ops.len(), 1);
    assert_eq!(unclaim_ops[0].task_id.as_deref(), Some("t1"));
}

#[test]
fn pause_produces_operation_log_entry() {
    let (_dir, wg_dir) = setup_wg(vec![make_task("t1", "Task 1", Status::Open)]);

    wg_ok(&wg_dir, &["pause", "t1"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let pause_ops = ops_with_op(&entries, "pause");
    assert_eq!(pause_ops.len(), 1);
    assert_eq!(pause_ops[0].task_id.as_deref(), Some("t1"));
}

#[test]
fn resume_produces_operation_log_entry() {
    let mut task = make_task("t1", "Task 1", Status::Open);
    task.paused = true;
    let (_dir, wg_dir) = setup_wg(vec![task]);

    wg_ok(&wg_dir, &["resume", "t1"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let resume_ops = ops_with_op(&entries, "resume");
    assert_eq!(resume_ops.len(), 1);
    assert_eq!(resume_ops[0].task_id.as_deref(), Some("t1"));
}

#[test]
fn archive_produces_operation_log_entry() {
    let mut task = make_task("t1", "Done task", Status::Done);
    task.completed_at = Some("2024-01-01T00:00:00Z".to_string());
    let (_dir, wg_dir) = setup_wg(vec![
        task,
        make_task("t2", "Open task", Status::Open),
    ]);

    wg_ok(&wg_dir, &["archive"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let archive_ops = ops_with_op(&entries, "archive");
    assert_eq!(archive_ops.len(), 1);
    assert_eq!(archive_ops[0].task_id, None); // batch op — task IDs in detail
    let task_ids = archive_ops[0].detail["task_ids"].as_array().unwrap();
    assert!(task_ids.iter().any(|v| v.as_str() == Some("t1")));
}

#[test]
fn gc_produces_operation_log_entry() {
    let (_dir, wg_dir) = setup_wg(vec![
        make_task("t1", "Abandoned task", Status::Abandoned),
        make_task("t2", "Open task", Status::Open),
    ]);

    wg_ok(&wg_dir, &["gc"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let gc_ops = ops_with_op(&entries, "gc");
    assert_eq!(gc_ops.len(), 1);
    assert_eq!(gc_ops[0].task_id, None); // batch op — task IDs in detail
    let removed = gc_ops[0].detail["removed"].as_array().unwrap();
    assert!(removed.iter().any(|v| v["id"].as_str() == Some("t1")));
}

// ── Full lifecycle: multiple commands produce correct sequence ──

#[test]
fn full_lifecycle_produces_ordered_operations() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

    // add -> claim -> pause -> resume -> done
    wg_ok(&wg_dir, &["add", "Lifecycle task", "--id", "lifecycle"]);
    wg_ok(&wg_dir, &["claim", "lifecycle", "--actor", "agent-1"]);
    wg_ok(&wg_dir, &["pause", "lifecycle"]);
    wg_ok(&wg_dir, &["resume", "lifecycle"]);
    wg_ok(&wg_dir, &["done", "lifecycle"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let ops: Vec<&str> = entries.iter().map(|e| e.op.as_str()).collect();

    assert_eq!(ops, vec!["add_task", "claim", "pause", "resume", "done"]);

    // All should reference the same task
    for entry in &entries {
        assert_eq!(entry.task_id.as_deref(), Some("lifecycle"));
    }

    // Timestamps should be monotonically non-decreasing
    for w in entries.windows(2) {
        assert!(w[0].timestamp <= w[1].timestamp, "timestamps not ordered");
    }
}

#[test]
fn fail_retry_done_lifecycle() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

    // add -> claim -> fail -> retry -> claim -> done
    wg_ok(&wg_dir, &["add", "Retry task", "--id", "retry-task"]);
    wg_ok(&wg_dir, &["claim", "retry-task"]);
    wg_ok(&wg_dir, &["fail", "retry-task", "--reason", "oops"]);
    wg_ok(&wg_dir, &["retry", "retry-task"]);
    wg_ok(&wg_dir, &["claim", "retry-task"]);
    wg_ok(&wg_dir, &["done", "retry-task"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let ops: Vec<&str> = entries.iter().map(|e| e.op.as_str()).collect();
    assert_eq!(
        ops,
        vec!["add_task", "claim", "fail", "retry", "claim", "done"]
    );
}

// ── Agent conversation archive tests ──

#[test]
fn agent_archive_on_done_preserves_files() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    // Create a task with an assigned agent
    let mut task = make_task("t1", "Agent task", Status::InProgress);
    task.assigned = Some("agent-abc".to_string());
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path).unwrap();

    // Create fake agent directory with prompt and output
    let agent_dir = wg_dir.join("agents").join("agent-abc");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(agent_dir.join("prompt.txt"), "Do the thing").unwrap();
    fs::write(agent_dir.join("output.log"), "I did the thing").unwrap();

    // Complete the task
    wg_ok(&wg_dir, &["done", "t1"]);

    // Verify archive exists
    let archive_base = wg_dir.join("log").join("agents").join("t1");
    assert!(archive_base.exists(), "Agent archive directory should exist");

    let attempts: Vec<_> = fs::read_dir(&archive_base)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(attempts.len(), 1, "Should have 1 archived attempt");

    let attempt_dir = attempts[0].path();
    assert_eq!(
        fs::read_to_string(attempt_dir.join("prompt.txt")).unwrap(),
        "Do the thing"
    );
    assert_eq!(
        fs::read_to_string(attempt_dir.join("output.txt")).unwrap(),
        "I did the thing"
    );
}

#[test]
fn agent_archive_on_fail_preserves_files() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    let mut task = make_task("t1", "Failing task", Status::InProgress);
    task.assigned = Some("agent-fail".to_string());
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(task));
    save_graph(&graph, &graph_path).unwrap();

    let agent_dir = wg_dir.join("agents").join("agent-fail");
    fs::create_dir_all(&agent_dir).unwrap();
    fs::write(agent_dir.join("prompt.txt"), "Fail prompt").unwrap();
    fs::write(agent_dir.join("output.log"), "Error output").unwrap();

    wg_ok(&wg_dir, &["fail", "t1", "--reason", "test fail"]);

    let archive_base = wg_dir.join("log").join("agents").join("t1");
    assert!(archive_base.exists(), "Agent archive should exist after fail");

    let attempts: Vec<_> = fs::read_dir(&archive_base)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .collect();
    assert_eq!(attempts.len(), 1);
}

// ── Log rotation tests ──

#[test]
fn rotation_triggers_under_high_volume() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");

    // Use a tiny threshold to force rotation
    let threshold = 100u64;

    for i in 0..50 {
        let entry = OperationEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            op: "bulk_test".to_string(),
            task_id: Some(format!("t{}", i)),
            actor: None,
            detail: serde_json::json!({ "index": i }),
        };
        provenance::append_operation(&wg_dir, &entry, threshold).unwrap();
    }

    // Should have rotated files
    let log_dir = provenance::log_dir(&wg_dir);
    let rotated: Vec<_> = fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".jsonl.zst"))
        .collect();

    assert!(
        !rotated.is_empty(),
        "Expected at least one rotated .zst file after 50 entries with 100-byte threshold"
    );

    // All 50 entries should be readable
    let all = provenance::read_all_operations(&wg_dir).unwrap();
    assert_eq!(all.len(), 50, "Expected 50 total entries across rotated + current");
}

#[test]
fn rotated_zstd_files_can_be_read_back() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    let threshold = 80u64;

    // Write entries that will trigger multiple rotations
    let total = 40;
    for i in 0..total {
        let entry = OperationEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            op: "readback_test".to_string(),
            task_id: Some(format!("task-{}", i)),
            actor: Some("test-actor".to_string()),
            detail: serde_json::json!({ "n": i }),
        };
        provenance::append_operation(&wg_dir, &entry, threshold).unwrap();
    }

    // Read all back
    let all = provenance::read_all_operations(&wg_dir).unwrap();
    assert_eq!(all.len(), total);

    // Verify ordering and content
    for (i, entry) in all.iter().enumerate() {
        assert_eq!(entry.op, "readback_test");
        assert_eq!(entry.task_id.as_deref(), Some(&format!("task-{}", i) as &str));
        assert_eq!(entry.actor.as_deref(), Some("test-actor"));
    }

    // Verify zstd magic bytes on rotated files
    let log_dir = provenance::log_dir(&wg_dir);
    for entry in fs::read_dir(&log_dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name().to_string_lossy().ends_with(".jsonl.zst") {
            let data = fs::read(entry.path()).unwrap();
            assert_eq!(&data[..4], &[0x28, 0xB5, 0x2F, 0xFD], "Invalid zstd magic");
        }
    }
}

#[test]
fn concurrent_writes_to_operation_log() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let dir = TempDir::new().unwrap();
    let wg_dir = Arc::new(dir.path().join(".workgraph"));

    // Create the log directory upfront
    fs::create_dir_all(provenance::log_dir(&wg_dir)).unwrap();

    let num_threads = 4;
    let entries_per_thread = 25;
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|tid| {
            let wg = Arc::clone(&wg_dir);
            let bar = Arc::clone(&barrier);
            thread::spawn(move || {
                bar.wait(); // Synchronize start
                for i in 0..entries_per_thread {
                    let entry = OperationEntry {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        op: "concurrent_write".to_string(),
                        task_id: Some(format!("t{}-{}", tid, i)),
                        actor: Some(format!("thread-{}", tid)),
                        detail: serde_json::Value::Null,
                    };
                    // Use large threshold to avoid rotation complicating things
                    provenance::append_operation(&wg, &entry, 100 * 1024 * 1024).unwrap();
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let all = provenance::read_all_operations(&wg_dir).unwrap();
    assert_eq!(
        all.len(),
        num_threads * entries_per_thread,
        "Expected {} entries from concurrent writes, got {}",
        num_threads * entries_per_thread,
        all.len()
    );

    // Each entry should be valid (no partial writes / corrupted JSON)
    for entry in &all {
        assert_eq!(entry.op, "concurrent_write");
        assert!(entry.task_id.is_some());
    }
}

#[test]
fn operation_log_survives_many_small_writes() {
    // Simulates what would happen with many rapid operations—no partial writes
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");

    for i in 0..200 {
        provenance::record(
            &wg_dir,
            "rapid_write",
            Some(&format!("t{}", i)),
            None,
            serde_json::Value::Null,
            provenance::DEFAULT_ROTATION_THRESHOLD,
        )
        .unwrap();
    }

    // Read back and verify all entries are valid JSONL
    let all = provenance::read_all_operations(&wg_dir).unwrap();
    assert_eq!(all.len(), 200);

    // Verify the raw file is valid JSONL (no partial/truncated lines)
    let ops_path = provenance::operations_path(&wg_dir);
    let content = fs::read_to_string(&ops_path).unwrap();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let _: OperationEntry = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("Invalid JSONL line: {}\nLine: {}", e, line));
    }
}

// ── Goal 2: Coherency — operation log and graph state are consistent ──

/// Check that the operation log is coherent with the current graph state.
///
/// Returns a list of coherency issues (empty = all good).
fn check_coherency(wg_dir: &Path) -> Vec<String> {
    let mut issues = Vec::new();

    let graph_path = wg_dir.join("graph.jsonl");
    let graph = match load_graph(&graph_path) {
        Ok(g) => g,
        Err(e) => {
            issues.push(format!("Failed to load graph: {}", e));
            return issues;
        }
    };

    let entries = match provenance::read_all_operations(wg_dir) {
        Ok(e) => e,
        Err(e) => {
            issues.push(format!("Failed to read operations: {}", e));
            return issues;
        }
    };

    // 1. Every task in the graph should have a corresponding add_task entry
    let added_ids: HashSet<String> = entries
        .iter()
        .filter(|e| e.op == "add_task")
        .filter_map(|e| e.task_id.clone())
        .collect();

    for task in graph.tasks() {
        if !added_ids.contains(&task.id) {
            issues.push(format!(
                "Task '{}' in graph but no add_task entry in operation log",
                task.id
            ));
        }
    }

    // 2. Tasks marked done should have a done entry
    for task in graph.tasks() {
        if task.status == Status::Done {
            let has_done = entries
                .iter()
                .any(|e| e.op == "done" && e.task_id.as_deref() == Some(&task.id));
            if !has_done {
                issues.push(format!(
                    "Task '{}' is Done but no done entry in operation log",
                    task.id
                ));
            }
        }
    }

    // 3. Tasks marked failed should have a fail entry
    for task in graph.tasks() {
        if task.status == Status::Failed {
            let has_fail = entries
                .iter()
                .any(|e| e.op == "fail" && e.task_id.as_deref() == Some(&task.id));
            if !has_fail {
                issues.push(format!(
                    "Task '{}' is Failed but no fail entry in operation log",
                    task.id
                ));
            }
        }
    }

    // 4. Tasks marked abandoned should have an abandon entry
    for task in graph.tasks() {
        if task.status == Status::Abandoned {
            let has_abandon = entries
                .iter()
                .any(|e| e.op == "abandon" && e.task_id.as_deref() == Some(&task.id));
            if !has_abandon {
                issues.push(format!(
                    "Task '{}' is Abandoned but no abandon entry in operation log",
                    task.id
                ));
            }
        }
    }

    // 5. Archived/gc'd tasks should NOT be in the graph
    //    (they had archive/gc entries but were removed from graph)
    let archived_ids: HashSet<String> = entries
        .iter()
        .filter(|e| e.op == "archive" || e.op == "gc")
        .filter_map(|e| e.task_id.clone())
        .collect();

    for archived_id in &archived_ids {
        if graph.get_task(archived_id).is_some() {
            // This is only an issue if there's no subsequent add_task that re-added it
            let last_op_for_task = entries
                .iter()
                .rev()
                .find(|e| e.task_id.as_deref() == Some(archived_id.as_str()));
            if let Some(last_op) = last_op_for_task
                && (last_op.op == "archive" || last_op.op == "gc") {
                    issues.push(format!(
                        "Task '{}' was {}'d but still in graph",
                        archived_id, last_op.op
                    ));
                }
        }
    }

    issues
}

#[test]
fn coherency_after_full_lifecycle() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

    // Perform a series of operations
    wg_ok(&wg_dir, &["add", "Task A", "--id", "task-a"]);
    wg_ok(&wg_dir, &["add", "Task B", "--id", "task-b"]);
    wg_ok(&wg_dir, &["add", "Task C", "--id", "task-c"]);

    // Complete A
    wg_ok(&wg_dir, &["done", "task-a"]);

    // Fail and abandon B
    wg_ok(&wg_dir, &["claim", "task-b"]);
    wg_ok(&wg_dir, &["fail", "task-b", "--reason", "broken"]);
    wg_ok(&wg_dir, &["abandon", "task-b"]);

    let issues = check_coherency(&wg_dir);
    assert!(
        issues.is_empty(),
        "Coherency issues found:\n{}",
        issues.join("\n")
    );
}

#[test]
fn coherency_detects_missing_add_entry() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    // Create graph with a task but no operation log entry for it
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("ghost", "Ghost task", Status::Open)));
    save_graph(&graph, &graph_path).unwrap();

    let issues = check_coherency(&wg_dir);
    assert!(
        issues.iter().any(|i| i.contains("ghost") && i.contains("no add_task")),
        "Expected missing add_task issue for ghost. Got: {:?}",
        issues
    );
}

#[test]
fn coherency_detects_done_without_done_entry() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();

    // Create graph with a done task but only an add entry (no done entry)
    let graph_path = wg_dir.join("graph.jsonl");
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("t1", "Done task", Status::Done)));
    save_graph(&graph, &graph_path).unwrap();

    // Write only an add_task entry
    provenance::record(
        &wg_dir,
        "add_task",
        Some("t1"),
        None,
        serde_json::Value::Null,
        provenance::DEFAULT_ROTATION_THRESHOLD,
    )
    .unwrap();

    let issues = check_coherency(&wg_dir);
    assert!(
        issues.iter().any(|i| i.contains("t1") && i.contains("Done") && i.contains("no done")),
        "Expected missing done entry issue. Got: {:?}",
        issues
    );
}

#[test]
fn coherency_after_archive_and_gc() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

    // Add tasks, complete one, abandon another
    wg_ok(&wg_dir, &["add", "Done task", "--id", "done-task"]);
    wg_ok(&wg_dir, &["add", "Abandon task", "--id", "abandon-task"]);
    wg_ok(&wg_dir, &["add", "Keep task", "--id", "keep-task"]);

    wg_ok(&wg_dir, &["done", "done-task"]);
    wg_ok(&wg_dir, &["abandon", "abandon-task"]);

    // Archive done tasks
    wg_ok(&wg_dir, &["archive"]);

    // GC abandoned tasks
    wg_ok(&wg_dir, &["gc"]);

    // Archived and gc'd tasks should NOT be in graph
    let graph = load_graph(wg_dir.join("graph.jsonl")).unwrap();
    assert!(graph.get_task("done-task").is_none(), "done-task should be archived");
    assert!(
        graph.get_task("abandon-task").is_none(),
        "abandon-task should be gc'd"
    );
    assert!(graph.get_task("keep-task").is_some(), "keep-task should remain");

    // Operation log should have all the entries
    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let ops: Vec<&str> = entries.iter().map(|e| e.op.as_str()).collect();

    assert!(ops.contains(&"add_task"));
    assert!(ops.contains(&"done"));
    assert!(ops.contains(&"abandon"));
    assert!(ops.contains(&"archive"));
    assert!(ops.contains(&"gc"));
}

// ── wg log --operations CLI test ──

#[test]
fn wg_log_operations_shows_entries() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

    wg_ok(&wg_dir, &["add", "Test task", "--id", "t1"]);
    wg_ok(&wg_dir, &["done", "t1"]);

    let output = wg_ok(&wg_dir, &["log", "--operations"]);
    assert!(output.contains("add_task"), "Should show add_task operation");
    assert!(output.contains("done"), "Should show done operation");
}

#[test]
fn wg_log_operations_json() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

    wg_ok(&wg_dir, &["add", "Test task", "--id", "t1"]);

    let output = wg_ok(&wg_dir, &["log", "--operations", "--json"]);
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&output)
        .unwrap_or_else(|e| panic!("Invalid JSON: {}\nOutput: {}", e, output));
    assert!(!parsed.is_empty());
    assert_eq!(parsed[0]["op"], "add_task");
}

// ── Edge cases ──

#[test]
fn empty_operation_log_is_readable() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    // Don't create any log directory
    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    assert!(entries.is_empty());
}

#[test]
fn edit_no_change_does_not_produce_log_entry() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

    wg_ok(&wg_dir, &["add", "No change task", "--id", "nc-task"]);

    // Edit with no actual changes
    wg_ok(&wg_dir, &["edit", "nc-task"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let edit_ops = ops_with_op(&entries, "edit");
    assert_eq!(
        edit_ops.len(),
        0,
        "Edit with no changes should not produce log entry"
    );
}

#[test]
fn multiple_tasks_archive_produces_multiple_entries() {
    let dir = TempDir::new().unwrap();
    let wg_dir = dir.path().join(".workgraph");
    fs::create_dir_all(&wg_dir).unwrap();
    fs::write(wg_dir.join("graph.jsonl"), "").unwrap();

    wg_ok(&wg_dir, &["add", "Done A", "--id", "done-a"]);
    wg_ok(&wg_dir, &["add", "Done B", "--id", "done-b"]);
    wg_ok(&wg_dir, &["done", "done-a"]);
    wg_ok(&wg_dir, &["done", "done-b"]);
    wg_ok(&wg_dir, &["archive"]);

    let entries = provenance::read_all_operations(&wg_dir).unwrap();
    let archive_ops = ops_with_op(&entries, "archive");
    assert_eq!(
        archive_ops.len(),
        1,
        "Archiving is a single batch operation"
    );

    let task_ids = archive_ops[0].detail["task_ids"].as_array().unwrap();
    let archived_ids: HashSet<_> = task_ids.iter().filter_map(|v| v.as_str()).collect();
    assert!(archived_ids.contains("done-a"));
    assert!(archived_ids.contains("done-b"));
}
