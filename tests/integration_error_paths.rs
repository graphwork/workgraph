//! Integration tests for error paths across the codebase.
//!
//! These test scenarios that are difficult to test at the unit level because they
//! require full graph persistence: missing/corrupted files, concurrent access,
//! invalid state transitions, dependency cycles, and loop edge edge cases.

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::{NamedTempFile, TempDir};
use workgraph::check::{check_all, check_cycles, check_orphans};
use workgraph::graph::{Node, Status, Task, WorkGraph};
use workgraph::parser::{ParseError, load_graph, save_graph};
use workgraph::query::{after, ready_tasks};

/// Helper: create a minimal open task.
fn make_task(id: &str) -> Task {
    Task {
        id: id.to_string(),
        title: format!("Task {}", id),
        ..Task::default()
    }
}

// ===========================================================================
// 1. Missing graph.jsonl — graceful error
// ===========================================================================

#[test]
fn test_load_missing_graph_file_returns_io_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nonexistent.jsonl");

    let result = load_graph(&path);
    assert!(
        result.is_err(),
        "Loading a missing file should return an error"
    );
    let err = result.unwrap_err();
    assert!(
        matches!(err, ParseError::Io(_)),
        "Error should be IO variant, got: {:?}",
        err
    );
}

#[test]
fn test_load_missing_graph_file_error_message_is_useful() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("does_not_exist.jsonl");

    let err = load_graph(&path).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("IO error") || msg.contains("No such file"),
        "Error message should mention the issue: {}",
        msg
    );
}

#[test]
fn test_save_to_readonly_directory_returns_error() {
    // save_graph uses atomic write (temp file + rename), so the directory
    // must be writable for temp file creation to succeed.
    use std::os::unix::fs::PermissionsExt;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graph.jsonl");

    let graph = WorkGraph::new();
    save_graph(&graph, &path).unwrap();

    // Make the directory read-only so temp file creation fails
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

    // Saving should fail because we can't create the temp file
    let mut graph2 = WorkGraph::new();
    graph2.add_node(Node::Task(make_task("t1")));
    let result = save_graph(&graph2, &path);

    // Restore permissions for cleanup
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(result.is_err(), "Saving to read-only directory should fail");
}

// ===========================================================================
// 2. Corrupted graph.jsonl — malformed JSON
// ===========================================================================

#[test]
fn test_load_completely_invalid_json() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(file, "this is not json at all").unwrap();

    let result = load_graph(file.path());
    assert!(result.is_err());
    match result.unwrap_err() {
        ParseError::Json { line, .. } => {
            assert_eq!(line, 1, "Error should report line 1");
        }
        other => panic!("Expected Json error, got: {:?}", other),
    }
}

#[test]
fn test_load_json_missing_required_fields() {
    let mut file = NamedTempFile::new().unwrap();
    // Valid JSON but missing required 'title' field for a task
    writeln!(file, r#"{{"kind":"task","id":"t1"}}"#).unwrap();

    let result = load_graph(file.path());
    assert!(result.is_err(), "Missing required fields should fail");
    assert!(matches!(
        result.unwrap_err(),
        ParseError::Json { line: 1, .. }
    ));
}

#[test]
fn test_load_json_wrong_type_for_field() {
    let mut file = NamedTempFile::new().unwrap();
    // 'status' should be a string, not a number
    writeln!(
        file,
        r#"{{"kind":"task","id":"t1","title":"Test","status":42}}"#
    )
    .unwrap();

    let result = load_graph(file.path());
    assert!(result.is_err(), "Wrong type for field should fail");
    assert!(matches!(
        result.unwrap_err(),
        ParseError::Json { line: 1, .. }
    ));
}

#[test]
fn test_load_json_invalid_status_value() {
    let mut file = NamedTempFile::new().unwrap();
    // 'status' should be one of the known variants, not "banana"
    writeln!(
        file,
        r#"{{"kind":"task","id":"t1","title":"Test","status":"banana"}}"#
    )
    .unwrap();

    let result = load_graph(file.path());
    assert!(result.is_err(), "Invalid status value should fail");
    assert!(matches!(
        result.unwrap_err(),
        ParseError::Json { line: 1, .. }
    ));
}

#[test]
fn test_load_corruption_on_second_line() {
    let mut file = NamedTempFile::new().unwrap();
    // First line is valid, second is corrupt
    writeln!(
        file,
        r#"{{"kind":"task","id":"t1","title":"Good Task","status":"open"}}"#
    )
    .unwrap();
    writeln!(file, "CORRUPT LINE").unwrap();

    let result = load_graph(file.path());
    assert!(result.is_err());
    match result.unwrap_err() {
        ParseError::Json { line, .. } => {
            assert_eq!(
                line, 2,
                "Error should report line 2 for second-line corruption"
            );
        }
        other => panic!("Expected Json error on line 2, got: {:?}", other),
    }
}

#[test]
fn test_load_truncated_json() {
    let mut file = NamedTempFile::new().unwrap();
    // Truncated JSON — missing closing brace
    writeln!(file, r#"{{"kind":"task","id":"t1","title":"Truncated"#).unwrap();

    let result = load_graph(file.path());
    assert!(result.is_err(), "Truncated JSON should fail");
    assert!(matches!(
        result.unwrap_err(),
        ParseError::Json { line: 1, .. }
    ));
}

#[test]
fn test_load_unknown_kind_fails() {
    let mut file = NamedTempFile::new().unwrap();
    // Unknown "kind" variant that isn't task, resource, or legacy actor
    writeln!(
        file,
        r#"{{"kind":"alien","id":"x1","name":"Unknown Entity"}}"#
    )
    .unwrap();

    let result = load_graph(file.path());
    assert!(result.is_err(), "Unknown kind should fail deserialization");
}

#[test]
fn test_load_mixed_valid_and_invalid_lines() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"{{"kind":"task","id":"t1","title":"Good","status":"open"}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"kind":"task","id":"t2","title":"Also Good","status":"done"}}"#
    )
    .unwrap();
    // Third line has invalid JSON
    writeln!(file, r#"{{"kind":"task","id":"t3","title":}}"#).unwrap();

    let result = load_graph(file.path());
    assert!(result.is_err());
    match result.unwrap_err() {
        ParseError::Json { line, .. } => {
            assert_eq!(line, 3, "Error should report the failing line number");
        }
        other => panic!("Expected Json error on line 3, got: {:?}", other),
    }
}

#[test]
fn test_load_empty_object_line() {
    let mut file = NamedTempFile::new().unwrap();
    // An empty JSON object has no "kind" discriminator
    writeln!(file, "{{}}").unwrap();

    let result = load_graph(file.path());
    assert!(
        result.is_err(),
        "Empty JSON object should fail (missing kind)"
    );
}

#[test]
fn test_load_duplicate_task_ids_last_wins() {
    let mut file = NamedTempFile::new().unwrap();
    writeln!(
        file,
        r#"{{"kind":"task","id":"dup","title":"First","status":"open"}}"#
    )
    .unwrap();
    writeln!(
        file,
        r#"{{"kind":"task","id":"dup","title":"Second","status":"done"}}"#
    )
    .unwrap();

    let graph = load_graph(file.path()).unwrap();
    // HashMap insert overwrites — last one wins
    let task = graph.get_task("dup").unwrap();
    assert_eq!(task.title, "Second");
    assert_eq!(task.status, Status::Done);
}

// ===========================================================================
// 3. Invalid state transitions
// ===========================================================================

#[test]
fn test_done_task_treated_as_done_by_blockers() {
    // Verify that a Done task unblocks its dependents regardless of how it got there
    let mut graph = WorkGraph::new();

    let mut t1 = make_task("t1");
    t1.status = Status::Done;

    let mut t2 = make_task("t2");
    t2.after = vec!["t1".to_string()];

    graph.add_node(Node::Task(t1));
    graph.add_node(Node::Task(t2));

    let ready = ready_tasks(&graph);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "t2");
}

#[test]
fn test_abandoned_task_unblocks_dependents() {
    // Abandoned is a terminal state — dependents should proceed
    let mut graph = WorkGraph::new();

    let mut t1 = make_task("t1");
    t1.status = Status::Abandoned;

    let mut t2 = make_task("t2");
    t2.after = vec!["t1".to_string()];

    graph.add_node(Node::Task(t1));
    graph.add_node(Node::Task(t2));

    // t2 should be ready because t1 is terminal (Abandoned)
    let ready = ready_tasks(&graph);
    assert!(
        ready.iter().any(|t| t.id == "t2"),
        "Task blocked by Abandoned task should be ready (terminal state)"
    );

    let blockers = after(&graph, "t2");
    assert!(
        blockers.is_empty(),
        "Abandoned task should not appear as blocker"
    );
}

#[test]
fn test_failed_task_unblocks_dependents() {
    // Failed is a terminal state — dependents should proceed
    let mut graph = WorkGraph::new();

    let mut t1 = make_task("t1");
    t1.status = Status::Failed;
    t1.failure_reason = Some("Test failure".to_string());

    let mut t2 = make_task("t2");
    t2.after = vec!["t1".to_string()];

    graph.add_node(Node::Task(t1));
    graph.add_node(Node::Task(t2));

    let ready = ready_tasks(&graph);
    assert!(
        ready.iter().any(|t| t.id == "t2"),
        "Task blocked by Failed task should be ready (terminal state)"
    );
}

#[test]
fn test_in_progress_task_blocks_dependents() {
    let mut graph = WorkGraph::new();

    let mut t1 = make_task("t1");
    t1.status = Status::InProgress;

    let mut t2 = make_task("t2");
    t2.after = vec!["t1".to_string()];

    graph.add_node(Node::Task(t1));
    graph.add_node(Node::Task(t2));

    let ready = ready_tasks(&graph);
    assert!(
        !ready.iter().any(|t| t.id == "t2"),
        "Task blocked by InProgress task should NOT be ready"
    );
}


#[test]
fn test_state_persistence_roundtrip_all_statuses() {
    // Verify that all status variants survive save/load roundtrip
    let file = NamedTempFile::new().unwrap();
    let statuses = [
        ("s-open", Status::Open),
        ("s-in-progress", Status::InProgress),
        ("s-done", Status::Done),
        ("s-blocked", Status::Blocked),
        ("s-failed", Status::Failed),
        ("s-abandoned", Status::Abandoned),
        ("s-done-2", Status::Done),
    ];

    let mut graph = WorkGraph::new();
    for (id, status) in &statuses {
        let mut task = make_task(id);
        task.status = *status;
        graph.add_node(Node::Task(task));
    }

    save_graph(&graph, file.path()).unwrap();
    let loaded = load_graph(file.path()).unwrap();

    for (id, expected_status) in &statuses {
        let task = loaded.get_task(id).unwrap();
        assert_eq!(
            &task.status, expected_status,
            "Status for {} should survive roundtrip",
            id
        );
    }
}

// ===========================================================================
// 4. Concurrent graph modifications (file locking)
// ===========================================================================

#[test]
fn test_concurrent_writes_produce_valid_graph() {
    let file = NamedTempFile::new().unwrap();
    let path = Arc::new(file.path().to_path_buf());

    // Initialize with one task
    let mut graph = WorkGraph::new();
    graph.add_node(Node::Task(make_task("seed")));
    save_graph(&graph, path.as_ref()).unwrap();

    let success_count = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    // 10 threads, each loading the graph, adding a unique task, and saving
    for i in 0..10 {
        let path = Arc::clone(&path);
        let success_count = Arc::clone(&success_count);

        handles.push(std::thread::spawn(move || {
            if let Ok(mut g) = load_graph(path.as_ref()) {
                g.add_node(Node::Task(make_task(&format!("thread-{}", i))));
                if save_graph(&g, path.as_ref()).is_ok() {
                    success_count.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // The graph must still be parseable — no corruption
    let final_graph = load_graph(path.as_ref()).unwrap();
    assert!(
        !final_graph.is_empty(),
        "Graph must contain at least the seed task"
    );
    assert!(
        success_count.load(Ordering::SeqCst) > 0,
        "At least some concurrent operations should succeed"
    );
}

#[test]
fn test_concurrent_read_write_no_corruption() {
    let file = NamedTempFile::new().unwrap();
    let path = Arc::new(file.path().to_path_buf());

    // Initialize
    let mut graph = WorkGraph::new();
    for i in 0..5 {
        graph.add_node(Node::Task(make_task(&format!("init-{}", i))));
    }
    save_graph(&graph, path.as_ref()).unwrap();

    let mut handles = vec![];

    // Readers
    for _ in 0..5 {
        let path = Arc::clone(&path);
        handles.push(std::thread::spawn(move || {
            let g = load_graph(path.as_ref()).unwrap();
            assert!(g.len() >= 5, "Readers should see at least initial tasks");
        }));
    }

    // Writers
    for i in 0..5 {
        let path = Arc::clone(&path);
        handles.push(std::thread::spawn(move || {
            if let Ok(mut g) = load_graph(path.as_ref()) {
                g.add_node(Node::Task(make_task(&format!("writer-{}", i))));
                let _ = save_graph(&g, path.as_ref());
            }
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // Final read: must not be corrupted
    let final_graph = load_graph(path.as_ref()).unwrap();
    assert!(
        final_graph.len() >= 5,
        "Graph must contain at least initial tasks after concurrent access"
    );
}

// ===========================================================================
// 5. Self-dependency detection
// ===========================================================================

#[test]
fn test_self_dependency_detected_as_cycle() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("self-dep");
    t.after = vec!["self-dep".to_string()];
    graph.add_node(Node::Task(t));

    let cycles = check_cycles(&graph);
    assert!(
        !cycles.is_empty(),
        "Self-dependency should be detected as a cycle"
    );
    // The cycle should be just the single task
    assert!(
        cycles.iter().any(|c| c.contains(&"self-dep".to_string())),
        "Cycle should include the self-referencing task"
    );
}

#[test]
fn test_self_dependency_makes_task_permanently_blocked() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("self-dep");
    t.after = vec!["self-dep".to_string()];
    graph.add_node(Node::Task(t));

    // A task that depends on itself can never be ready
    let ready = ready_tasks(&graph);
    assert!(
        !ready.iter().any(|t| t.id == "self-dep"),
        "Self-dependent task should never be ready"
    );
}

#[test]
fn test_self_dependency_detected_by_check_all() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("self-dep");
    t.after = vec!["self-dep".to_string()];
    graph.add_node(Node::Task(t));

    let result = check_all(&graph);
    // Cycles are warnings, not errors — ok is still true
    assert!(result.ok, "Cycles are warnings; ok should still be true");
    assert!(
        !result.cycles.is_empty(),
        "check_all should report the cycle"
    );
}

#[test]
fn test_self_dependency_persists_through_save_load() {
    let file = NamedTempFile::new().unwrap();

    let mut graph = WorkGraph::new();
    let mut t = make_task("self-dep");
    t.after = vec!["self-dep".to_string()];
    graph.add_node(Node::Task(t));

    save_graph(&graph, file.path()).unwrap();
    let loaded = load_graph(file.path()).unwrap();

    let task = loaded.get_task("self-dep").unwrap();
    assert_eq!(
        task.after,
        vec!["self-dep".to_string()],
        "Self-dependency should survive save/load"
    );

    let cycles = check_cycles(&loaded);
    assert!(!cycles.is_empty());
}

// ===========================================================================
// 6. Multi-level cycles in after
// ===========================================================================

#[test]
fn test_two_node_cycle() {
    let mut graph = WorkGraph::new();

    let mut t1 = make_task("a");
    t1.after = vec!["b".to_string()];
    let mut t2 = make_task("b");
    t2.after = vec!["a".to_string()];

    graph.add_node(Node::Task(t1));
    graph.add_node(Node::Task(t2));

    let cycles = check_cycles(&graph);
    assert!(!cycles.is_empty(), "Two-node cycle should be detected");

    // Neither task should be ready
    let ready = ready_tasks(&graph);
    assert!(ready.is_empty(), "No tasks in a cycle should be ready");

    let result = check_all(&graph);
    // Cycles are warnings, not errors — ok is still true
    assert!(result.ok, "Cycles are warnings; ok should still be true");
    assert!(
        !result.cycles.is_empty(),
        "Two-node cycle should be reported"
    );
}

#[test]
fn test_three_node_cycle() {
    let mut graph = WorkGraph::new();

    let mut t1 = make_task("a");
    t1.after = vec!["c".to_string()];
    let mut t2 = make_task("b");
    t2.after = vec!["a".to_string()];
    let mut t3 = make_task("c");
    t3.after = vec!["b".to_string()];

    graph.add_node(Node::Task(t1));
    graph.add_node(Node::Task(t2));
    graph.add_node(Node::Task(t3));

    let cycles = check_cycles(&graph);
    assert!(!cycles.is_empty(), "Three-node cycle should be detected");

    let ready = ready_tasks(&graph);
    assert!(
        ready.is_empty(),
        "No tasks in a three-node cycle should be ready"
    );
}

#[test]
fn test_five_node_cycle() {
    let mut graph = WorkGraph::new();

    // a -> b -> c -> d -> e -> a (all after their predecessor)
    let ids = ["a", "b", "c", "d", "e"];
    for i in 0..ids.len() {
        let mut t = make_task(ids[i]);
        let prev = ids[(i + ids.len() - 1) % ids.len()]; // previous in the ring
        t.after = vec![prev.to_string()];
        graph.add_node(Node::Task(t));
    }

    let cycles = check_cycles(&graph);
    assert!(!cycles.is_empty(), "Five-node cycle should be detected");

    let ready = ready_tasks(&graph);
    assert!(
        ready.is_empty(),
        "No tasks in a five-node cycle should be ready"
    );
}

#[test]
fn test_cycle_with_branch() {
    // Graph: a <-> b (cycle), c -> a (c depends on a, not in cycle)
    let mut graph = WorkGraph::new();

    let mut a = make_task("a");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c");
    c.after = vec!["a".to_string()];

    graph.add_node(Node::Task(a));
    graph.add_node(Node::Task(b));
    graph.add_node(Node::Task(c));

    let cycles = check_cycles(&graph);
    assert!(!cycles.is_empty(), "Cycle a<->b should be detected");

    // c is blocked by a, which is in a cycle, so c can never be ready
    let ready = ready_tasks(&graph);
    assert!(ready.is_empty());
}

#[test]
fn test_cycle_persists_through_save_load() {
    let file = NamedTempFile::new().unwrap();

    let mut graph = WorkGraph::new();
    let mut t1 = make_task("a");
    t1.after = vec!["b".to_string()];
    let mut t2 = make_task("b");
    t2.after = vec!["a".to_string()];

    graph.add_node(Node::Task(t1));
    graph.add_node(Node::Task(t2));

    save_graph(&graph, file.path()).unwrap();
    let loaded = load_graph(file.path()).unwrap();

    let cycles = check_cycles(&loaded);
    assert!(
        !cycles.is_empty(),
        "Cycle should survive save/load roundtrip"
    );
}

#[test]
fn test_diamond_dependency_no_false_cycle() {
    // Diamond: a <- b, a <- c, b <- d, c <- d (d depends on b and c, both depend on a)
    // This is NOT a cycle — it's a valid DAG diamond
    let mut graph = WorkGraph::new();

    let a = make_task("a");
    let mut b = make_task("b");
    b.after = vec!["a".to_string()];
    let mut c = make_task("c");
    c.after = vec!["a".to_string()];
    let mut d = make_task("d");
    d.after = vec!["b".to_string(), "c".to_string()];

    graph.add_node(Node::Task(a));
    graph.add_node(Node::Task(b));
    graph.add_node(Node::Task(c));
    graph.add_node(Node::Task(d));

    let cycles = check_cycles(&graph);
    assert!(
        cycles.is_empty(),
        "Diamond dependency should NOT be flagged as a cycle"
    );

    // Only 'a' should be ready (b and c depend on a, d depends on b and c)
    let ready = ready_tasks(&graph);
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "a");

    let result = check_all(&graph);
    assert!(result.ok);
}


// ===========================================================================
// Additional edge cases: orphan references, mixed validation
// ===========================================================================

#[test]
fn test_orphan_after_reference() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("orphan-dep");
    t.after = vec!["does-not-exist".to_string()];
    graph.add_node(Node::Task(t));

    let orphans = check_orphans(&graph);
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].from, "orphan-dep");
    assert_eq!(orphans[0].to, "does-not-exist");
    assert_eq!(orphans[0].relation, "after");

    let result = check_all(&graph);
    assert!(!result.ok, "Orphan reference should make graph invalid");
}

#[test]
fn test_orphan_blocks_reference() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("orphan-blocks");
    t.before = vec!["does-not-exist".to_string()];
    graph.add_node(Node::Task(t));

    let orphans = check_orphans(&graph);
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].relation, "before");
}

#[test]
fn test_orphan_requires_reference() {
    let mut graph = WorkGraph::new();

    let mut t = make_task("orphan-req");
    t.requires = vec!["phantom-resource".to_string()];
    graph.add_node(Node::Task(t));

    let orphans = check_orphans(&graph);
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].relation, "requires");
}

#[test]
fn test_ready_tasks_treats_missing_blocker_as_unblocked() {
    // If after references a task that doesn't exist, ready_tasks treats it as unblocked
    // (the blocker is gone, so the dependency is satisfied)
    let mut graph = WorkGraph::new();

    let mut t = make_task("has-phantom-dep");
    t.after = vec!["phantom".to_string()];
    graph.add_node(Node::Task(t));

    let ready = ready_tasks(&graph);
    assert_eq!(
        ready.len(),
        1,
        "Task with nonexistent blocker should be ready (orphan treated as unblocked)"
    );
    assert_eq!(ready[0].id, "has-phantom-dep");
}

#[test]
fn test_graph_with_multiple_error_types() {
    // A graph that has: cycle and orphan reference simultaneously
    let mut graph = WorkGraph::new();

    // Cycle: a <-> b
    let mut a = make_task("a");
    a.after = vec!["b".to_string()];
    let mut b = make_task("b");
    b.after = vec!["a".to_string()];

    // Orphan: c references nonexistent
    let mut c = make_task("c");
    c.after = vec!["nonexistent".to_string()];

    graph.add_node(Node::Task(a));
    graph.add_node(Node::Task(b));
    graph.add_node(Node::Task(c));

    let result = check_all(&graph);
    assert!(!result.ok);
    assert!(!result.cycles.is_empty(), "Should detect cycle");
    assert!(!result.orphan_refs.is_empty(), "Should detect orphan");
}

#[test]
fn test_large_graph_cycle_detection_performance() {
    // Ensure cycle detection works on a larger graph without hanging.
    // Chain of 100 tasks, last one creates a cycle back to the first.
    let mut graph = WorkGraph::new();

    for i in 0..100 {
        let mut t = make_task(&format!("t{}", i));
        if i > 0 {
            t.after = vec![format!("t{}", i - 1)];
        }
        graph.add_node(Node::Task(t));
    }

    // Add cycle: t0 after t99
    graph.get_task_mut("t0").unwrap().after = vec!["t99".to_string()];

    let cycles = check_cycles(&graph);
    assert!(!cycles.is_empty(), "100-node cycle should be detected");

    // All tasks are in a cycle, none should be ready
    let ready = ready_tasks(&graph);
    assert!(ready.is_empty());
}
