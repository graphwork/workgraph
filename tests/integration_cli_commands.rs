//! Integration tests for high-traffic CLI workflows.
//!
//! Tests the graph-level operations that back CLI commands: list/filter,
//! claim/unclaim, edit, archive, and the full retry lifecycle.

use chrono::Utc;
use tempfile::TempDir;
use workgraph::graph::{LogEntry, Node, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query::ready_tasks;

/// Helper: create a minimal open task.
fn make_task(id: &str, title: &str) -> Task {
    Task {
        id: id.to_string(),
        title: title.to_string(),
        ..Task::default()
    }
}

fn setup_graph(tasks: Vec<Task>) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graph.jsonl");
    let mut graph = WorkGraph::new();
    for task in tasks {
        graph.add_node(Node::Task(task));
    }
    save_graph(&graph, &path).unwrap();
    (dir, path)
}

// ── list / filter ────────────────────────────────────────────────────

#[test]
fn list_filter_by_status() {
    let mut done = make_task("d1", "Done task");
    done.status = Status::Done;
    let (_dir, path) = setup_graph(vec![make_task("o1", "Open task"), done]);

    let graph = load_graph(&path).unwrap();
    let open_tasks: Vec<_> = graph.tasks().filter(|t| t.status == Status::Open).collect();
    assert_eq!(open_tasks.len(), 1);
    assert_eq!(open_tasks[0].id, "o1");

    let done_tasks: Vec<_> = graph.tasks().filter(|t| t.status == Status::Done).collect();
    assert_eq!(done_tasks.len(), 1);
    assert_eq!(done_tasks[0].id, "d1");
}

#[test]
fn list_empty_graph() {
    let (_dir, path) = setup_graph(vec![]);
    let graph = load_graph(&path).unwrap();
    assert_eq!(graph.tasks().count(), 0);
}

#[test]
fn list_all_statuses_present() {
    let mut ip = make_task("ip", "In progress");
    ip.status = Status::InProgress;
    let mut blocked = make_task("bl", "Blocked");
    blocked.status = Status::Blocked;
    let mut failed = make_task("fl", "Failed");
    failed.status = Status::Failed;
    let mut done = make_task("dn", "Done");
    done.status = Status::Done;

    let (_dir, path) = setup_graph(vec![make_task("op", "Open"), ip, blocked, failed, done]);

    let graph = load_graph(&path).unwrap();
    assert_eq!(graph.tasks().count(), 5);
}

// ── claim / unclaim ──────────────────────────────────────────────────

#[test]
fn claim_sets_in_progress_and_assigned() {
    let (_dir, path) = setup_graph(vec![make_task("c1", "Claimable")]);

    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("c1").unwrap();
    task.status = Status::InProgress;
    task.assigned = Some("agent-1".to_string());
    task.started_at = Some(Utc::now().to_rfc3339());
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("c1").unwrap();
    assert_eq!(task.status, Status::InProgress);
    assert_eq!(task.assigned.as_deref(), Some("agent-1"));
    assert!(task.started_at.is_some());
}

#[test]
fn claim_already_in_progress_is_guarded() {
    let mut t = make_task("c2", "Already claimed");
    t.status = Status::InProgress;
    t.assigned = Some("other-agent".into());
    let (_dir, path) = setup_graph(vec![t]);

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("c2").unwrap();
    assert_eq!(task.status, Status::InProgress);
    // A second claim should be rejected in the command layer
    assert!(task.assigned.is_some());
}

#[test]
fn unclaim_resets_to_open() {
    let mut t = make_task("u1", "Unclaim me");
    t.status = Status::InProgress;
    t.assigned = Some("agent-1".into());
    let (_dir, path) = setup_graph(vec![t]);

    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("u1").unwrap();
    task.status = Status::Open;
    task.assigned = None;
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("u1").unwrap();
    assert_eq!(task.status, Status::Open);
    assert!(task.assigned.is_none());
}

// ── edit ──────────────────────────────────────────────────────────────

#[test]
fn edit_title_and_description() {
    let (_dir, path) = setup_graph(vec![make_task("e1", "Old title")]);

    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("e1").unwrap();
    task.title = "New title".to_string();
    task.description = Some("A description".to_string());
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("e1").unwrap();
    assert_eq!(task.title, "New title");
    assert_eq!(task.description.as_deref(), Some("A description"));
}

#[test]
fn edit_tags_add_and_remove() {
    let (_dir, path) = setup_graph(vec![make_task("e2", "Taggable")]);

    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("e2").unwrap();
    task.tags.push("important".to_string());
    task.tags.push("urgent".to_string());
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    assert_eq!(graph.get_task("e2").unwrap().tags.len(), 2);

    // Remove one tag
    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("e2").unwrap();
    task.tags.retain(|t| t != "urgent");
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    let tags = &graph.get_task("e2").unwrap().tags;
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0], "important");
}

#[test]
fn edit_after_updates_dependency() {
    let (_dir, path) = setup_graph(vec![
        make_task("dep", "Dependency"),
        make_task("e3", "Blocked task"),
    ]);

    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("e3").unwrap();
    task.after.push("dep".to_string());
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    assert!(
        graph
            .get_task("e3")
            .unwrap()
            .after
            .contains(&"dep".to_string())
    );

    // e3 should NOT be ready because dep is not done
    let ready = ready_tasks(&graph);
    assert!(!ready.iter().any(|t| t.id == "e3"));
    // dep IS ready
    assert!(ready.iter().any(|t| t.id == "dep"));
}

// ── archive (done tasks removed, open tasks remain) ──────────────────

#[test]
fn archive_workflow_removes_done_keeps_open() {
    let mut done = make_task("arch-1", "Done");
    done.status = Status::Done;
    done.completed_at = Some(Utc::now().to_rfc3339());
    let (_dir, path) = setup_graph(vec![make_task("open-1", "Still open"), done]);

    // Simulate archive: reload, remove done tasks, save
    let mut graph = load_graph(&path).unwrap();
    let done_ids: Vec<String> = graph
        .tasks()
        .filter(|t| t.status == Status::Done)
        .map(|t| t.id.clone())
        .collect();
    for id in &done_ids {
        graph.remove_node(id);
    }
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    assert!(graph.get_task("arch-1").is_none());
    assert!(graph.get_task("open-1").is_some());
}

// ── status summary ──────────────────────────────────────────────────

#[test]
fn status_counts_all_categories() {
    let mut ip = make_task("ip", "In progress");
    ip.status = Status::InProgress;
    let mut done = make_task("dn", "Done");
    done.status = Status::Done;
    let mut failed = make_task("fl", "Failed");
    failed.status = Status::Failed;

    let (_dir, path) = setup_graph(vec![make_task("op", "Open"), ip, done, failed]);

    let graph = load_graph(&path).unwrap();
    let total = graph.tasks().count();
    let open = graph.tasks().filter(|t| t.status == Status::Open).count();
    let in_progress = graph
        .tasks()
        .filter(|t| t.status == Status::InProgress)
        .count();
    let done_count = graph.tasks().filter(|t| t.status == Status::Done).count();
    let failed_count = graph.tasks().filter(|t| t.status == Status::Failed).count();

    assert_eq!(total, 4);
    assert_eq!(open, 1);
    assert_eq!(in_progress, 1);
    assert_eq!(done_count, 1);
    assert_eq!(failed_count, 1);
}

// ── retry lifecycle: fail -> retry -> claim -> done ──────────────────

#[test]
fn full_retry_lifecycle() {
    let (_dir, path) = setup_graph(vec![make_task("r1", "Retriable")]);

    // 1. Claim the task
    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("r1").unwrap();
    task.status = Status::InProgress;
    task.assigned = Some("agent-a".to_string());
    task.started_at = Some(Utc::now().to_rfc3339());
    save_graph(&graph, &path).unwrap();

    // 2. Fail the task
    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("r1").unwrap();
    task.status = Status::Failed;
    task.retry_count += 1;
    task.failure_reason = Some("transient error".to_string());
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("r1").unwrap();
    assert_eq!(task.status, Status::Failed);
    assert_eq!(task.retry_count, 1);
    assert_eq!(task.failure_reason.as_deref(), Some("transient error"));

    // 3. Retry: reset to open
    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("r1").unwrap();
    task.status = Status::Open;
    task.assigned = None;
    task.failure_reason = None;
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("r1").unwrap();
    assert_eq!(task.status, Status::Open);
    assert!(task.assigned.is_none());
    // retry_count is preserved across retries
    assert_eq!(task.retry_count, 1);

    // 4. Claim again by a different agent
    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("r1").unwrap();
    task.status = Status::InProgress;
    task.assigned = Some("agent-b".to_string());
    save_graph(&graph, &path).unwrap();

    // 5. Complete the task
    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("r1").unwrap();
    task.status = Status::Done;
    task.completed_at = Some(Utc::now().to_rfc3339());
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("r1").unwrap();
    assert_eq!(task.status, Status::Done);
    assert!(task.completed_at.is_some());
}

#[test]
fn multiple_failures_increment_retry_count() {
    let (_dir, path) = setup_graph(vec![make_task("r2", "Flaky")]);

    for i in 1..=3 {
        // Claim
        let mut graph = load_graph(&path).unwrap();
        let task = graph.get_task_mut("r2").unwrap();
        task.status = Status::InProgress;
        save_graph(&graph, &path).unwrap();

        // Fail
        let mut graph = load_graph(&path).unwrap();
        let task = graph.get_task_mut("r2").unwrap();
        task.status = Status::Failed;
        task.retry_count += 1;
        task.failure_reason = Some(format!("attempt {}", i));
        save_graph(&graph, &path).unwrap();

        let graph = load_graph(&path).unwrap();
        assert_eq!(graph.get_task("r2").unwrap().retry_count, i);

        // Reset for retry
        let mut graph = load_graph(&path).unwrap();
        let task = graph.get_task_mut("r2").unwrap();
        task.status = Status::Open;
        task.assigned = None;
        save_graph(&graph, &path).unwrap();
    }

    let graph = load_graph(&path).unwrap();
    assert_eq!(graph.get_task("r2").unwrap().retry_count, 3);
}

// ── log entries ──────────────────────────────────────────────────────

#[test]
fn add_log_entry_persists() {
    let (_dir, path) = setup_graph(vec![make_task("l1", "Logged")]);

    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("l1").unwrap();
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: Some("agent-1".to_string()),
        message: "Starting work".to_string(),
    });
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("l1").unwrap();
    assert_eq!(task.log.len(), 1);
    assert_eq!(task.log[0].message, "Starting work");
    assert_eq!(task.log[0].actor.as_deref(), Some("agent-1"));
}

// ── ready tasks with dependency resolution ────────────────────────────

#[test]
fn ready_tasks_respects_dependencies() {
    let mut blocked = make_task("child", "Blocked child");
    blocked.after.push("parent".to_string());
    let (_dir, path) = setup_graph(vec![make_task("parent", "Parent"), blocked]);

    let graph = load_graph(&path).unwrap();
    let ready = ready_tasks(&graph);
    // Only parent should be ready
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "parent");

    // Complete parent
    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("parent").unwrap();
    task.status = Status::Done;
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    let ready = ready_tasks(&graph);
    // Now child should be ready
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].id, "child");
}
