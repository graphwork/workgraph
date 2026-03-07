//! hold/unhold commands — atomic subtree pause/resume with provenance tracking.
//!
//! Unlike `wg pause` (single task), `wg hold` pauses a task and all transitive
//! dependents atomically. `wg unhold` reverses exactly what `wg hold` paused,
//! using provenance records to know which tasks to resume.

use anyhow::Result;
use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use workgraph::graph::{LogEntry, Status};

/// Hold: pause a task and all its transitive dependents atomically.
/// Kills agents on in-progress tasks. Terminal tasks (done/abandoned) are skipped.
pub fn hold(dir: &Path, id: &str, dry_run: bool) -> Result<()> {
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
    let mut affected = vec![id.to_string()];
    let mut deps: Vec<String> = visited.into_iter().collect();
    deps.sort();
    affected.extend(deps);

    if dry_run {
        println!("Dry run — would hold:");
        for tid in &affected {
            let task = graph.get_task(tid);
            let skip = task.map_or(false, |t| {
                t.status == Status::Done || t.status == Status::Abandoned || t.paused
            });
            if skip {
                println!("  skip: {} (terminal or already paused)", tid);
            } else {
                println!("  hold: {}", tid);
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

    let held_tasks = super::mutate_workgraph(dir, |graph| {
        let mut held = Vec::new();
        for tid in &affected {
            let task = graph.get_task_mut_or_err(tid)?;
            // Skip terminal or already-paused tasks
            if task.status == Status::Done || task.status == Status::Abandoned || task.paused {
                continue;
            }
            task.paused = true;
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: None,
                message: format!("Task held via `wg hold {}`", id),
                ..Default::default()
            });
            held.push(tid.clone());
        }
        Ok(held)
    })?;

    super::notify_graph_changed(dir);

    // Record provenance — the held_tasks list is what unhold uses to reverse
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "hold",
        Some(id),
        None,
        serde_json::json!({
            "held_tasks": held_tasks,
        }),
        config.log.rotation_threshold,
    );

    for tid in &held_tasks {
        println!("  held: {}", tid);
    }
    println!("Hold: {} task(s) paused", held_tasks.len());

    Ok(())
}

/// Unhold: resume exactly the tasks that were paused by the matching `wg hold` call.
/// Uses provenance records to determine which tasks to unpause.
pub fn unhold(dir: &Path, id: &str, dry_run: bool) -> Result<()> {
    // Find the most recent hold provenance entry for this task
    let entries = workgraph::provenance::read_all_operations(dir).unwrap_or_default();
    let hold_entry = entries
        .iter()
        .rev()
        .find(|e| e.op == "hold" && e.task_id.as_deref() == Some(id));

    let held_tasks: Vec<String> = match hold_entry {
        Some(entry) => entry
            .detail
            .get("held_tasks")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default(),
        None => {
            anyhow::bail!(
                "No hold record found for '{}'. Use `wg resume` for manual unpause.",
                id
            );
        }
    };

    if held_tasks.is_empty() {
        println!("No tasks were held for '{}' — nothing to unhold", id);
        return Ok(());
    }

    if dry_run {
        println!("Dry run — would unhold:");
        for tid in &held_tasks {
            println!("  unhold: {}", tid);
        }
        return Ok(());
    }

    let unholded = super::mutate_workgraph(dir, |graph| {
        let mut resumed = Vec::new();
        for tid in &held_tasks {
            if let Ok(task) = graph.get_task_mut_or_err(tid) {
                if task.paused {
                    task.paused = false;
                    task.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: None,
                        message: format!("Task unhold via `wg unhold {}`", id),
                        ..Default::default()
                    });
                    resumed.push(tid.clone());
                }
            }
        }
        Ok(resumed)
    })?;

    super::notify_graph_changed(dir);

    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "unhold",
        Some(id),
        None,
        serde_json::json!({
            "unholded_tasks": unholded,
        }),
        config.log.rotation_threshold,
    );

    for tid in &unholded {
        println!("  resumed: {}", tid);
    }
    println!("Unhold: {} task(s) resumed", unholded.len());

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
    fn test_hold_pauses_subtree() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", Status::Open);
        c.after = vec!["b".to_string()];
        setup(dir, vec![a, b, c]);

        hold(dir, "a", false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert!(graph.get_task("a").unwrap().paused);
        assert!(graph.get_task("b").unwrap().paused);
        assert!(graph.get_task("c").unwrap().paused);
    }

    #[test]
    fn test_hold_skips_terminal() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Done);
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", Status::Open);
        c.after = vec!["b".to_string()];
        setup(dir, vec![a, b, c]);

        hold(dir, "a", false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert!(graph.get_task("a").unwrap().paused);
        assert!(!graph.get_task("b").unwrap().paused);
        assert!(graph.get_task("c").unwrap().paused);
    }

    #[test]
    fn test_hold_dry_run_no_changes() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        setup(dir, vec![a, b]);

        hold(dir, "a", true).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert!(!graph.get_task("a").unwrap().paused);
        assert!(!graph.get_task("b").unwrap().paused);
    }

    #[test]
    fn test_unhold_resumes_held_tasks() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        let mut c = make_task("c", Status::Open);
        c.after = vec!["b".to_string()];
        setup(dir, vec![a, b, c]);

        hold(dir, "a", false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert!(graph.get_task("a").unwrap().paused);
        assert!(graph.get_task("b").unwrap().paused);
        assert!(graph.get_task("c").unwrap().paused);

        unhold(dir, "a", false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert!(!graph.get_task("a").unwrap().paused);
        assert!(!graph.get_task("b").unwrap().paused);
        assert!(!graph.get_task("c").unwrap().paused);
    }

    #[test]
    fn test_unhold_only_resumes_what_was_held() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        b.paused = true; // pre-paused, not by hold
        let mut c = make_task("c", Status::Open);
        c.after = vec!["b".to_string()];
        setup(dir, vec![a, b, c]);

        hold(dir, "a", false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert!(graph.get_task("a").unwrap().paused);
        assert!(graph.get_task("b").unwrap().paused);
        assert!(graph.get_task("c").unwrap().paused);

        unhold(dir, "a", false).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert!(!graph.get_task("a").unwrap().paused);
        assert!(graph.get_task("b").unwrap().paused); // still paused
        assert!(!graph.get_task("c").unwrap().paused);
    }

    #[test]
    fn test_unhold_no_hold_record_errors() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Open)]);

        let err = unhold(dir, "a", false).unwrap_err();
        assert!(format!("{:#}", err).contains("No hold record"));
    }

    #[test]
    fn test_hold_nonexistent_task_errors() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Open)]);

        let err = hold(dir, "nonexistent", false).unwrap_err();
        assert!(format!("{:#}", err).contains("not found"));
    }

    #[test]
    fn test_hold_records_provenance() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        setup(dir, vec![a, b]);

        hold(dir, "a", false).unwrap();

        let entries = workgraph::provenance::read_all_operations(dir).unwrap();
        let hold_ops: Vec<_> = entries.iter().filter(|e| e.op == "hold").collect();
        assert_eq!(hold_ops.len(), 1);
        assert_eq!(hold_ops[0].task_id.as_deref(), Some("a"));
        let held = hold_ops[0].detail["held_tasks"].as_array().unwrap();
        assert!(held.iter().any(|v| v.as_str() == Some("a")));
        assert!(held.iter().any(|v| v.as_str() == Some("b")));
    }

    #[test]
    fn test_unhold_dry_run_no_changes() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        setup(dir, vec![a, b]);

        hold(dir, "a", false).unwrap();
        unhold(dir, "a", true).unwrap();

        let graph = load_graph(graph_path(dir)).unwrap();
        assert!(graph.get_task("a").unwrap().paused);
        assert!(graph.get_task("b").unwrap().paused);
    }

    #[test]
    fn test_hold_unhold_records_provenance() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let a = make_task("a", Status::Open);
        let mut b = make_task("b", Status::Open);
        b.after = vec!["a".to_string()];
        setup(dir, vec![a, b]);

        hold(dir, "a", false).unwrap();
        unhold(dir, "a", false).unwrap();

        let entries = workgraph::provenance::read_all_operations(dir).unwrap();
        assert!(entries.iter().any(|e| e.op == "hold"));
        assert!(entries.iter().any(|e| e.op == "unhold"));
    }
}
