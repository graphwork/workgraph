//! cascade-stop command — abandon (or hold) a task and all its transitive dependents.

use anyhow::Result;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::graph::{LogEntry, Status};

/// Run cascade-stop: abandon the target task and all transitive dependents.
/// With `hold`, pause them instead. Terminal tasks (done/abandoned) are skipped.
pub fn run(dir: &Path, id: &str, hold: bool, dry_run: bool) -> Result<()> {
    // Build reverse index and collect transitive dependents
    let (graph, _) = super::load_workgraph(dir)?;
    graph.get_task_or_err(id)?;

    let mut reverse_index = HashMap::new();
    for task in graph.tasks() {
        for dep in &task.after {
            reverse_index
                .entry(dep.clone())
                .or_insert_with(Vec::new)
                .push(task.id.clone());
        }
        for downstream_id in &task.before {
            reverse_index
                .entry(task.id.clone())
                .or_insert_with(Vec::new)
                .push(downstream_id.clone());
        }
    }

    let mut visited = HashSet::new();
    super::collect_transitive_dependents(&reverse_index, id, &mut visited);
    // Include the seed task itself
    let mut affected = vec![id.to_string()];
    let mut deps: Vec<String> = visited.into_iter().collect();
    deps.sort();
    affected.extend(deps);

    if dry_run {
        let action = if hold { "hold (pause)" } else { "abandon" };
        println!("Dry run — would {}:", action);
        for tid in &affected {
            let status = graph.get_task(tid).map(|t| t.status);
            let skip = matches!(status, Some(Status::Done) | Some(Status::Abandoned));
            if skip {
                println!("  skip (terminal): {}", tid);
            } else {
                println!("  {}: {}", action, tid);
            }
        }
        return Ok(());
    }

    // Kill agents on in-progress tasks before mutating
    for tid in &affected {
        if let Some(task) = graph.get_task(tid) {
            if task.status == Status::InProgress {
                kill_agent_for_task(dir, tid);
            }
        }
    }

    let action = if hold { "hold" } else { "cascade-stop" };
    let results = super::mutate_workgraph(dir, |graph| {
        let mut results = Vec::new();
        for tid in &affected {
            let task = graph.get_task_mut_or_err(tid)?;
            // Skip terminal tasks
            if task.status == Status::Done || task.status == Status::Abandoned {
                continue;
            }
            let prev_status = task.status;
            if hold {
                if !task.paused {
                    task.paused = true;
                    task.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: None,
                        message: format!("Task held via `wg cascade-stop --hold {}`", id),
                        ..Default::default()
                    });
                    results.push((tid.clone(), format!("{} → held", prev_status)));
                }
            } else {
                task.status = Status::Abandoned;
                task.failure_reason = Some(format!("cascade-stop from '{}'", id));
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: None,
                    message: format!("Task abandoned via `wg cascade-stop {}`", id),
                    ..Default::default()
                });
                results.push((tid.clone(), format!("{} → abandoned", prev_status)));
            }
        }
        Ok(results)
    })?;

    super::notify_graph_changed(dir);

    // Record provenance
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        action,
        Some(id),
        None,
        serde_json::json!({
            "hold": hold,
            "affected": affected,
            "results": results.iter().map(|(t, r)| serde_json::json!({"task": t, "result": r})).collect::<Vec<_>>(),
        }),
        config.log.rotation_threshold,
    );

    for (tid, result) in &results {
        println!("  {}: {}", tid, result);
    }
    println!(
        "Cascade {}: {} task(s) affected",
        if hold { "hold" } else { "stop" },
        results.len()
    );

    Ok(())
}

/// Best-effort kill of an agent working on a given task.
fn kill_agent_for_task(dir: &Path, task_id: &str) {
    let Ok(registry) = workgraph::service::AgentRegistry::load(dir) else {
        return;
    };
    let Some(agent) = registry.get_agent_by_task(task_id) else {
        return;
    };
    if !agent.is_alive() {
        return;
    }
    let pid = agent.pid;
    let _ = super::kill_process_graceful(pid, 5);
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
    fn test_cascade_stop_abandons_downstream() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", Status::InProgress);
        c.after = vec!["b".to_string()];
        setup(dir, vec![a, b, c]);

        run(dir, "a", false, false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("a").unwrap().status, Status::Abandoned);
        assert_eq!(graph.get_task("b").unwrap().status, Status::Abandoned);
        assert_eq!(graph.get_task("c").unwrap().status, Status::Abandoned);
    }

    #[test]
    fn test_cascade_stop_hold_pauses_instead() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        setup(dir, vec![a, b]);

        run(dir, "a", true, false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert!(graph.get_task("a").unwrap().paused);
        assert!(graph.get_task("b").unwrap().paused);
        assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
    }

    #[test]
    fn test_cascade_stop_skips_terminal() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Done);
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", Status::Abandoned);
        c.after = vec!["a".to_string()];
        let mut d = make_task("d", Status::Open);
        d.after = vec!["b".to_string()];
        setup(dir, vec![a, b, c, d]);

        run(dir, "a", false, false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("a").unwrap().status, Status::Abandoned);
        assert_eq!(graph.get_task("b").unwrap().status, Status::Done);
        assert_eq!(graph.get_task("c").unwrap().status, Status::Abandoned);
        assert_eq!(graph.get_task("d").unwrap().status, Status::Abandoned);
    }

    #[test]
    fn test_cascade_stop_dry_run_no_changes() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        setup(dir, vec![a, b]);

        run(dir, "a", false, true).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
    }

    #[test]
    fn test_cascade_stop_single_task_no_deps() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Open)]);

        run(dir, "a", false, false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("a").unwrap().status, Status::Abandoned);
    }

    #[test]
    fn test_cascade_stop_nonexistent_task_errors() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Open)]);

        let err = run(dir, "nonexistent", false, false).unwrap_err();
        assert!(format!("{:#}", err).contains("not found"));
    }

    #[test]
    fn test_cascade_stop_hold_skips_already_paused() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        b.paused = true;
        setup(dir, vec![a, b]);

        run(dir, "a", true, false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert!(graph.get_task("a").unwrap().paused);
        assert!(graph.get_task("b").unwrap().paused);
        assert!(graph.get_task("a").unwrap().log.last().unwrap().message.contains("held"));
    }

    #[test]
    fn test_cascade_stop_records_provenance() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        setup(dir, vec![a, b]);

        run(dir, "a", false, false).unwrap();

        let entries = workgraph::provenance::read_all_operations(dir).unwrap();
        let ops: Vec<_> = entries.iter().filter(|e| e.op == "cascade-stop").collect();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].task_id.as_deref(), Some("a"));
    }

    #[test]
    fn test_cascade_stop_diamond_shape() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let root = make_task("root", Status::Open);
        let mut left = make_task("left", Status::Open);
        left.after = vec!["root".to_string()];
        let mut right = make_task("right", Status::Open);
        right.after = vec!["root".to_string()];
        let mut join = make_task("join", Status::Open);
        join.after = vec!["left".to_string(), "right".to_string()];
        setup(dir, vec![root, left, right, join]);

        run(dir, "root", false, false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        for id in &["root", "left", "right", "join"] {
            assert_eq!(
                graph.get_task(id).unwrap().status,
                Status::Abandoned,
                "{} should be abandoned",
                id
            );
        }
    }
}
