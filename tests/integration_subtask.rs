//! Integration tests for --subtask flag on wg add.
//!
//! Covers: subtask creation, parent wait condition, child completion triggers parent resume,
//! child failure triggers parent resume, error cases.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;
use workgraph::graph::{Node, Status, Task, WaitCondition, WaitSpec, WorkGraph};
use workgraph::parser::{load_graph, save_graph};

// ── helpers ──────────────────────────────────────────────────────────────

fn make_task(id: &str) -> Task {
    Task {
        id: id.to_string(),
        title: id.to_string(),
        ..Task::default()
    }
}

fn setup_workgraph(dir: &Path, tasks: Vec<Task>) -> PathBuf {
    let wg_dir = dir.join(".workgraph");
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

fn wg_cmd_env(
    wg_dir: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = Command::new(wg_binary());
    cmd.arg("--dir")
        .arg(wg_dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("Failed to run wg {:?}: {}", args, e))
}

fn wg_cmd(wg_dir: &Path, args: &[&str]) -> std::process::Output {
    wg_cmd_env(wg_dir, args, &[])
}

fn graph_path(wg_dir: &Path) -> PathBuf {
    wg_dir.join("graph.jsonl")
}

// ── tests ────────────────────────────────────────────────────────────────

#[test]
fn subtask_creates_child_and_parks_parent() {
    let dir = TempDir::new().unwrap();
    let mut parent = make_task("parent-task");
    parent.status = Status::InProgress;
    let wg_dir = setup_workgraph(dir.path(), vec![parent]);

    let output = wg_cmd_env(
        &wg_dir,
        &["add", "Research something", "--subtask", "--no-place"],
        &[("WG_TASK_ID", "parent-task")],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "wg add --subtask should succeed.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
    assert!(
        stdout.contains("subtask"),
        "Should mention subtask in output: {}",
        stdout
    );

    let graph = load_graph(&graph_path(&wg_dir)).unwrap();

    let child = graph.get_task("research-something").unwrap();
    assert_eq!(child.status, Status::Open);
    assert!(
        child.after.is_empty(),
        "Child should not depend on parent: {:?}",
        child.after
    );

    let parent = graph.get_task("parent-task").unwrap();
    assert_eq!(parent.status, Status::Waiting);
    assert!(parent.wait_condition.is_some());

    match parent.wait_condition.as_ref().unwrap() {
        WaitSpec::Any(conditions) => {
            assert_eq!(conditions.len(), 2);
            assert!(conditions.contains(&WaitCondition::TaskStatus {
                task_id: "research-something".to_string(),
                status: Status::Done,
            }));
            assert!(conditions.contains(&WaitCondition::TaskStatus {
                task_id: "research-something".to_string(),
                status: Status::Failed,
            }));
        }
        other => panic!("Expected WaitSpec::Any, got {:?}", other),
    }
}

#[test]
fn subtask_fails_without_wg_task_id() {
    let dir = TempDir::new().unwrap();
    let wg_dir = setup_workgraph(dir.path(), vec![]);

    let output = wg_cmd(&wg_dir, &["add", "Child task", "--subtask"]);
    assert!(
        !output.status.success(),
        "wg add --subtask should fail without WG_TASK_ID"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("WG_TASK_ID"),
        "Error should mention WG_TASK_ID: {}",
        stderr
    );
}

#[test]
fn subtask_fails_if_parent_not_in_progress() {
    let dir = TempDir::new().unwrap();
    let parent = make_task("parent-task");
    let wg_dir = setup_workgraph(dir.path(), vec![parent]);

    let output = wg_cmd_env(
        &wg_dir,
        &["add", "Child task", "--subtask"],
        &[("WG_TASK_ID", "parent-task")],
    );
    assert!(
        !output.status.success(),
        "wg add --subtask should fail when parent is not in-progress"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("in-progress"),
        "Error should mention in-progress: {}",
        stderr
    );
}

#[test]
fn subtask_child_does_not_depend_on_parent() {
    let dir = TempDir::new().unwrap();
    let mut parent = make_task("parent-task");
    parent.status = Status::InProgress;
    let wg_dir = setup_workgraph(dir.path(), vec![parent]);

    let output = wg_cmd_env(
        &wg_dir,
        &["add", "Quick research", "--subtask"],
        &[("WG_TASK_ID", "parent-task"), ("WG_AGENT_ID", "agent-1")],
    );
    assert!(
        output.status.success(),
        "Should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let graph = load_graph(&graph_path(&wg_dir)).unwrap();
    let child = graph.get_task("quick-research").unwrap();

    assert!(
        !child.after.contains(&"parent-task".to_string()),
        "Subtask child should NOT depend on parent. after: {:?}",
        child.after
    );
    assert_eq!(child.status, Status::Open);
}

#[test]
fn subtask_with_explicit_after() {
    let dir = TempDir::new().unwrap();
    let mut parent = make_task("parent-task");
    parent.status = Status::InProgress;
    let mut dep = make_task("dep-task");
    dep.status = Status::Done;
    let wg_dir = setup_workgraph(dir.path(), vec![parent, dep]);

    let output = wg_cmd_env(
        &wg_dir,
        &[
            "add",
            "Subtask with dep",
            "--subtask",
            "--after",
            "dep-task",
        ],
        &[("WG_TASK_ID", "parent-task"), ("WG_AGENT_ID", "agent-1")],
    );
    assert!(
        output.status.success(),
        "Should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let graph = load_graph(&graph_path(&wg_dir)).unwrap();
    let child = graph.get_task("subtask-with-dep").unwrap();

    assert!(child.after.contains(&"dep-task".to_string()));
    assert!(!child.after.contains(&"parent-task".to_string()));

    let parent = graph.get_task("parent-task").unwrap();
    assert_eq!(parent.status, Status::Waiting);
}

#[test]
fn subtask_coordinator_resumes_parent_on_child_done() {
    let dir = TempDir::new().unwrap();

    let mut parent = make_task("parent-task");
    parent.status = Status::Waiting;
    parent.wait_condition = Some(WaitSpec::Any(vec![
        WaitCondition::TaskStatus {
            task_id: "child-task".to_string(),
            status: Status::Done,
        },
        WaitCondition::TaskStatus {
            task_id: "child-task".to_string(),
            status: Status::Failed,
        },
    ]));

    let mut child = make_task("child-task");
    child.status = Status::Done;
    child.log.push(workgraph::graph::LogEntry {
        timestamp: chrono::Utc::now().to_rfc3339(),
        actor: Some("agent-2".to_string()),
        user: None,
        message: "Completed research: found 3 candidate libraries.".to_string(),
    });
    child.artifacts = vec!["research-output.md".to_string()];

    let wg_dir = setup_workgraph(dir.path(), vec![parent, child]);

    use workgraph::parser::modify_graph;
    let gp = graph_path(&wg_dir);
    let mut did_change = false;
    let _ = modify_graph(&gp, |graph| {
        let task = graph.get_task("parent-task").unwrap();
        assert_eq!(task.status, Status::Waiting);

        if let Some(ref spec) = task.wait_condition {
            let conditions = match spec {
                WaitSpec::All(c) | WaitSpec::Any(c) => c,
            };
            for cond in conditions {
                if let WaitCondition::TaskStatus { task_id, status } = cond {
                    if let Some(dep) = graph.get_task(task_id) {
                        if dep.status == *status {
                            did_change = true;
                        }
                    }
                }
            }
        }

        if did_change {
            let t = graph.get_task_mut("parent-task").unwrap();
            t.status = Status::Open;
            t.wait_condition = None;
        }
        did_change
    });

    assert!(did_change, "Wait condition should be satisfied");

    let graph = load_graph(&gp).unwrap();
    let parent = graph.get_task("parent-task").unwrap();
    assert_eq!(parent.status, Status::Open);
    assert!(parent.wait_condition.is_none());
}

#[test]
fn subtask_coordinator_resumes_parent_on_child_failed() {
    let dir = TempDir::new().unwrap();

    let mut parent = make_task("parent-task");
    parent.status = Status::Waiting;
    parent.wait_condition = Some(WaitSpec::Any(vec![
        WaitCondition::TaskStatus {
            task_id: "child-task".to_string(),
            status: Status::Done,
        },
        WaitCondition::TaskStatus {
            task_id: "child-task".to_string(),
            status: Status::Failed,
        },
    ]));

    let mut child = make_task("child-task");
    child.status = Status::Failed;
    child.failure_reason = Some("Build failed with 3 errors".to_string());

    let wg_dir = setup_workgraph(dir.path(), vec![parent, child]);

    use workgraph::parser::modify_graph;
    let gp = graph_path(&wg_dir);
    let mut did_change = false;
    let _ = modify_graph(&gp, |graph| {
        if let Some(ref spec) = graph.get_task("parent-task").unwrap().wait_condition {
            let conditions = match spec {
                WaitSpec::Any(c) => c,
                _ => panic!("Expected Any"),
            };
            for cond in conditions {
                if let WaitCondition::TaskStatus { task_id, status } = cond {
                    if let Some(dep) = graph.get_task(task_id) {
                        if dep.status == *status {
                            did_change = true;
                        }
                    }
                }
            }
        }
        if did_change {
            let t = graph.get_task_mut("parent-task").unwrap();
            t.status = Status::Open;
            t.wait_condition = None;
        }
        did_change
    });

    assert!(did_change, "Wait condition should be satisfied when child fails");
    let graph = load_graph(&gp).unwrap();
    assert_eq!(
        graph.get_task("parent-task").unwrap().status,
        Status::Open
    );
}
