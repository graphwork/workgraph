//! Reopen command — transition Done tasks back to Open

use anyhow::Result;
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};

/// Reopen a Done task (and optionally cascade downstream).
pub fn run(dir: &Path, id: &str, cascade: bool) -> Result<()> {
    let reopened = super::mutate_workgraph(dir, |graph| {
        let task = graph.get_task_or_err(id)?;
        if task.status != Status::Done {
            anyhow::bail!(
                "Task '{}' is not done (status: {}). Only done tasks can be reopened.",
                id,
                task.status
            );
        }

        let mut reopened = vec![id.to_string()];
        reopen_task(graph, id, "Reopened manually via `wg reopen`");

        if cascade {
            let downstream = collect_done_downstream(graph, id);
            for did in &downstream {
                reopen_task(
                    graph,
                    did,
                    &format!("Reopened: upstream '{}' was reopened (cascade)", id),
                );
            }
            reopened.extend(downstream);
        }

        Ok(reopened)
    })?;

    super::notify_graph_changed(dir);

    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "reopen",
        Some(id),
        None,
        serde_json::json!({ "cascade": cascade, "reopened": reopened }),
        config.log.rotation_threshold,
    );

    println!("Reopened '{}'", id);
    if reopened.len() > 1 {
        for rid in &reopened[1..] {
            println!("  cascaded → '{}'", rid);
        }
    }

    Ok(())
}

/// Reset a single task from Done to Open, clearing assignment fields.
pub fn reopen_task(graph: &mut workgraph::graph::WorkGraph, id: &str, reason: &str) {
    if let Some(task) = graph.get_task_mut(id) {
        task.status = Status::Open;
        task.assigned = None;
        task.started_at = None;
        task.completed_at = None;
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            message: reason.to_string(),
            ..Default::default()
        });
    }
}

/// Collect all transitive downstream Done tasks via `before` edges (BFS).
pub fn collect_done_downstream(
    graph: &workgraph::graph::WorkGraph,
    start_id: &str,
) -> Vec<String> {
    let mut result = Vec::new();
    let mut queue = std::collections::VecDeque::new();
    let mut visited = std::collections::HashSet::new();
    visited.insert(start_id.to_string());

    // Seed with direct `before` edges of the start task
    if let Some(task) = graph.get_task(start_id) {
        for b in &task.before {
            if visited.insert(b.clone()) {
                queue.push_back(b.clone());
            }
        }
    }

    while let Some(tid) = queue.pop_front() {
        if let Some(task) = graph.get_task(&tid) {
            if task.status == Status::Done {
                result.push(tid.clone());
                // Continue traversal through this task's `before` edges
                for b in &task.before {
                    if visited.insert(b.clone()) {
                        queue.push_back(b.clone());
                    }
                }
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::{load_graph, save_graph};

    fn graph_path(dir: &Path) -> std::path::PathBuf {
        dir.join("graph.jsonl")
    }

    fn make_task(id: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            status,
            ..Task::default()
        }
    }

    fn setup(dir: &Path, tasks: Vec<Task>) {
        fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
    }

    #[test]
    fn test_reopen_done_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut t = make_task("a", Status::Done);
        t.assigned = Some("agent-1".to_string());
        t.started_at = Some("2025-01-01T00:00:00Z".to_string());
        t.completed_at = Some("2025-01-01T01:00:00Z".to_string());
        setup(dir, vec![t]);

        run(dir, "a", false).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        let task = graph.get_task("a").unwrap();
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.assigned, None);
        assert_eq!(task.started_at, None);
        assert_eq!(task.completed_at, None);
        assert!(task.log.last().unwrap().message.contains("Reopened"));
    }

    #[test]
    fn test_reopen_non_done_errors() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Open)]);

        let err = run(dir, "a", false).unwrap_err();
        assert!(format!("{:#}", err).contains("not done"));
    }

    #[test]
    fn test_reopen_cascade() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        // a -> b -> c (all done), b.before = [c], a.before = [b]
        let mut a = make_task("a", Status::Done);
        a.before = vec!["b".to_string()];
        let mut b = make_task("b", Status::Done);
        b.after = vec!["a".to_string()];
        b.before = vec!["c".to_string()];
        let mut c = make_task("c", Status::Done);
        c.after = vec!["b".to_string()];

        setup(dir, vec![a, b, c]);

        run(dir, "a", true).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("c").unwrap().status, Status::Open);
    }

    #[test]
    fn test_reopen_cascade_skips_non_done() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mut a = make_task("a", Status::Done);
        a.before = vec!["b".to_string()];
        // b is Open, so cascade should stop
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        b.before = vec!["c".to_string()];
        let mut c = make_task("c", Status::Done);
        c.after = vec!["b".to_string()];

        setup(dir, vec![a, b, c]);

        run(dir, "a", true).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
        // b was already Open, should stay
        assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
        // c should NOT be reopened because cascade stops at non-Done b
        assert_eq!(graph.get_task("c").unwrap().status, Status::Done);
    }

    #[test]
    fn test_reopen_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Done)]);

        let err = run(dir, "nonexistent", false).unwrap_err();
        assert!(format!("{:#}", err).contains("not found"));
    }
}
