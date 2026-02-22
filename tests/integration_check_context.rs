//! Integration tests for check, context, and artifact workflows.
//!
//! Tests the graph validation (check), dependency context resolution, and
//! artifact lifecycle that back `wg check`, `wg context`, and `wg artifact`.

use tempfile::TempDir;
use workgraph::check::check_all;
use workgraph::graph::{Node, Resource, Status, Task, WorkGraph};
use workgraph::parser::{load_graph, save_graph};

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

fn setup_graph_with_nodes(nodes: Vec<Node>) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("graph.jsonl");
    let mut graph = WorkGraph::new();
    for node in nodes {
        graph.add_node(node);
    }
    save_graph(&graph, &path).unwrap();
    (dir, path)
}

// ── check: orphan references ────────────────────────────────────────

#[test]
fn check_detects_orphan_after() {
    let mut task = make_task("t1", "Depends on missing");
    task.after.push("nonexistent".to_string());
    let (_dir, path) = setup_graph(vec![task]);

    let graph = load_graph(&path).unwrap();
    let result = check_all(&graph);

    assert!(
        !result.orphan_refs.is_empty(),
        "Should detect orphan after reference"
    );
    assert!(result.orphan_refs.iter().any(|o| o.to == "nonexistent"));
}

#[test]
fn check_detects_orphan_requires() {
    let mut task = make_task("t1", "Needs missing resource");
    task.requires.push("missing-resource".to_string());
    let (_dir, path) = setup_graph(vec![task]);

    let graph = load_graph(&path).unwrap();
    let result = check_all(&graph);

    assert!(
        !result.orphan_refs.is_empty(),
        "Should detect orphan requires reference"
    );
    assert!(
        result
            .orphan_refs
            .iter()
            .any(|o| o.to == "missing-resource")
    );
}

#[test]
fn check_clean_graph_passes() {
    let mut parent = make_task("parent", "Parent");
    parent.status = Status::Done;
    let mut child = make_task("child", "Child");
    child.after.push("parent".to_string());
    let (_dir, path) = setup_graph(vec![parent, child]);

    let graph = load_graph(&path).unwrap();
    let result = check_all(&graph);

    assert!(result.orphan_refs.is_empty());
    assert!(result.cycles.is_empty());
}

// ── check: resource references ────────────────────────────────────────

#[test]
fn check_valid_resource_reference() {
    let mut task = make_task("t1", "Needs DB");
    task.requires.push("db".to_string());
    let resource = Resource {
        id: "db".to_string(),
        name: Some("Database".to_string()),
        resource_type: Some("database".to_string()),
        available: Some(1.0),
        unit: None,
    };
    let (_dir, path) = setup_graph_with_nodes(vec![Node::Task(task), Node::Resource(resource)]);

    let graph = load_graph(&path).unwrap();
    let result = check_all(&graph);

    assert!(
        result.orphan_refs.is_empty(),
        "Valid resource reference should not be an orphan"
    );
}

// ── context: dependency artifacts ─────────────────────────────────────

#[test]
fn context_collects_artifacts_from_dependencies() {
    let mut dep = make_task("dep", "Dependency");
    dep.status = Status::Done;
    dep.artifacts = vec!["output.json".to_string(), "report.md".to_string()];

    let mut child = make_task("child", "Dependent");
    child.after.push("dep".to_string());
    child.inputs = vec!["output.json".to_string()];

    let (_dir, path) = setup_graph(vec![dep, child]);

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("child").unwrap();

    // Collect artifacts from dependencies
    let mut all_artifacts = std::collections::HashSet::new();
    for dep_id in &task.after {
        if let Some(dep_task) = graph.get_task(dep_id) {
            for artifact in &dep_task.artifacts {
                all_artifacts.insert(artifact.clone());
            }
        }
    }

    assert!(all_artifacts.contains("output.json"));
    assert!(all_artifacts.contains("report.md"));

    // Check missing inputs
    let missing: Vec<_> = task
        .inputs
        .iter()
        .filter(|input| !all_artifacts.contains(*input))
        .collect();
    assert!(missing.is_empty(), "output.json should be available");
}

#[test]
fn context_identifies_missing_inputs() {
    let mut dep = make_task("dep", "Dependency");
    dep.status = Status::Done;
    dep.artifacts = vec!["output.json".to_string()];

    let mut child = make_task("child", "Dependent");
    child.after.push("dep".to_string());
    child.inputs = vec!["output.json".to_string(), "missing.csv".to_string()];

    let (_dir, path) = setup_graph(vec![dep, child]);

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("child").unwrap();

    let mut all_artifacts = std::collections::HashSet::new();
    for dep_id in &task.after {
        if let Some(dep_task) = graph.get_task(dep_id) {
            for artifact in &dep_task.artifacts {
                all_artifacts.insert(artifact.clone());
            }
        }
    }

    let missing: Vec<_> = task
        .inputs
        .iter()
        .filter(|input| !all_artifacts.contains(*input))
        .cloned()
        .collect();
    assert_eq!(missing, vec!["missing.csv".to_string()]);
}

#[test]
fn context_empty_when_no_dependencies() {
    let task = make_task("standalone", "No deps");
    let (_dir, path) = setup_graph(vec![task]);

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("standalone").unwrap();

    assert!(task.after.is_empty());
    assert!(task.inputs.is_empty());
}

// ── artifact lifecycle ────────────────────────────────────────────────

#[test]
fn artifact_add_and_persist() {
    let (_dir, path) = setup_graph(vec![make_task("t1", "Artifact test")]);

    // Add artifact
    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("t1").unwrap();
    task.artifacts.push("result.json".to_string());
    save_graph(&graph, &path).unwrap();

    // Verify persistence
    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("t1").unwrap();
    assert_eq!(task.artifacts, vec!["result.json".to_string()]);
}

#[test]
fn artifact_add_duplicate_is_idempotent() {
    let (_dir, path) = setup_graph(vec![make_task("t1", "Artifact test")]);

    // Add artifact
    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("t1").unwrap();
    task.artifacts.push("result.json".to_string());
    save_graph(&graph, &path).unwrap();

    // Try to add same artifact - check for existing
    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("t1").unwrap();
    if !task.artifacts.contains(&"result.json".to_string()) {
        task.artifacts.push("result.json".to_string());
    }
    save_graph(&graph, &path).unwrap();

    // Should still have only one
    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("t1").unwrap();
    assert_eq!(task.artifacts.len(), 1);
}

#[test]
fn artifact_remove() {
    let mut task = make_task("t1", "Artifact test");
    task.artifacts = vec!["a.txt".to_string(), "b.txt".to_string()];
    let (_dir, path) = setup_graph(vec![task]);

    // Remove artifact
    let mut graph = load_graph(&path).unwrap();
    let task = graph.get_task_mut("t1").unwrap();
    task.artifacts.retain(|a| a != "a.txt");
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    let task = graph.get_task("t1").unwrap();
    assert_eq!(task.artifacts, vec!["b.txt".to_string()]);
}

#[test]
fn artifact_multiple_tasks_independent() {
    let (_dir, path) = setup_graph(vec![make_task("t1", "Task 1"), make_task("t2", "Task 2")]);

    // Add different artifacts to different tasks
    let mut graph = load_graph(&path).unwrap();
    graph
        .get_task_mut("t1")
        .unwrap()
        .artifacts
        .push("t1-output.txt".to_string());
    graph
        .get_task_mut("t2")
        .unwrap()
        .artifacts
        .push("t2-output.txt".to_string());
    save_graph(&graph, &path).unwrap();

    let graph = load_graph(&path).unwrap();
    assert_eq!(
        graph.get_task("t1").unwrap().artifacts,
        vec!["t1-output.txt"]
    );
    assert_eq!(
        graph.get_task("t2").unwrap().artifacts,
        vec!["t2-output.txt"]
    );
}

// ── check with complex dependency graphs ──────────────────────────────

#[test]
fn check_diamond_dependency_no_cycle() {
    // A -> B, A -> C, B -> D, C -> D (diamond)
    let mut a = make_task("a", "A");
    let mut b = make_task("b", "B");
    b.after.push("a".to_string());
    let mut c = make_task("c", "C");
    c.after.push("a".to_string());
    let mut d = make_task("d", "D");
    d.after.push("b".to_string());
    d.after.push("c".to_string());

    // Set up blocks (reverse edges)
    a.before = vec!["b".to_string(), "c".to_string()];
    b.before = vec!["d".to_string()];
    c.before = vec!["d".to_string()];

    let (_dir, path) = setup_graph(vec![a, b, c, d]);

    let graph = load_graph(&path).unwrap();
    let result = check_all(&graph);

    assert!(result.cycles.is_empty(), "Diamond should not be a cycle");
    assert!(result.orphan_refs.is_empty());
}

