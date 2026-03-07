//! Reset command — clean-slate task reset regardless of current status.
//!
//! Unlike `wg retry` (failed tasks only, preserves retry_count), reset is a full clean slate.
//! Clears all execution state while preserving identity and structure.

use anyhow::Result;
use chrono::Utc;
use std::collections::HashSet;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::service::AgentRegistry;

/// Reset a task to clean open state.
pub fn run(
    dir: &Path,
    id: &str,
    downstream: bool,
    retract: bool,
    dry_run: bool,
) -> Result<()> {
    // Collect tasks to reset
    let mut to_reset = vec![id.to_string()];

    if downstream {
        let (graph, _) = super::load_workgraph(dir)?;
        let mut reverse_index = std::collections::HashMap::new();
        for task in graph.tasks() {
            for dep in &task.after {
                reverse_index
                    .entry(dep.clone())
                    .or_insert_with(Vec::new)
                    .push(task.id.clone());
            }
        }
        let mut visited = HashSet::new();
        super::collect_transitive_dependents(&reverse_index, id, &mut visited);
        to_reset.extend(visited);
    }

    // Retract: find tasks created by agents working on tasks being reset
    let mut retracted = Vec::new();
    if retract {
        let entries = workgraph::provenance::read_all_operations(dir).unwrap_or_default();
        // Find agent_ids that worked on the tasks being reset (from claim operations)
        let reset_set: HashSet<&str> = to_reset.iter().map(|s| s.as_str()).collect();
        let mut agent_ids = HashSet::new();
        for entry in &entries {
            if entry.op == "claim" {
                if let Some(ref tid) = entry.task_id {
                    if reset_set.contains(tid.as_str()) {
                        if let Some(ref actor) = entry.actor {
                            agent_ids.insert(actor.clone());
                        }
                    }
                }
            }
        }
        // Find tasks created by those agents
        for entry in &entries {
            if entry.op == "add_task" {
                if let Some(aid) = entry.detail.get("agent_id").and_then(|v| v.as_str()) {
                    if agent_ids.contains(aid) {
                        if let Some(ref tid) = entry.task_id {
                            if !reset_set.contains(tid.as_str()) {
                                retracted.push(tid.clone());
                            }
                        }
                    }
                }
            }
        }
    }

    if dry_run {
        println!("Dry run — would reset:");
        for tid in &to_reset {
            println!("  reset: {}", tid);
        }
        for tid in &retracted {
            println!("  retract (abandon): {}", tid);
        }
        return Ok(());
    }

    // Kill agents on in-progress tasks (best-effort, before graph mutation)
    for tid in &to_reset {
        kill_agent_for_task(dir, tid);
    }

    // Retract: abandon created tasks
    for tid in &retracted {
        let _ = super::abandon::run(dir, tid, Some("retracted by wg reset"));
    }

    // Perform the reset inside a single atomic mutation
    let prev_statuses = super::mutate_workgraph(dir, |graph| {
        let mut statuses = Vec::new();
        for tid in &to_reset {
            let task = graph.get_task_mut_or_err(tid)?;
            let prev = task.status;
            reset_task(task);
            statuses.push((tid.clone(), prev));
        }
        Ok(statuses)
    })?;

    super::notify_graph_changed(dir);

    // Record provenance
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "reset",
        Some(id),
        None,
        serde_json::json!({
            "downstream": downstream,
            "retract": retract,
            "reset_tasks": to_reset,
            "retracted_tasks": retracted,
        }),
        config.log.rotation_threshold,
    );

    // Print results
    for (tid, prev) in &prev_statuses {
        println!("Reset '{}' ({} → open)", tid, prev);
    }
    for tid in &retracted {
        println!("  retracted → '{}'", tid);
    }

    Ok(())
}

/// Clear all execution state on a task, preserving identity and structure.
fn reset_task(task: &mut workgraph::graph::Task) {
    task.status = Status::Open;
    task.assigned = None;
    task.started_at = None;
    task.completed_at = None;
    task.failure_reason = None;
    task.retry_count = 0;
    task.loop_iteration = 0;
    task.cycle_failure_restarts = 0;
    task.session_id = None;
    task.checkpoint = None;
    task.wait_condition = None;
    task.token_usage = None;
    task.paused = false;

    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: None,
        message: "Task reset to clean open state via `wg reset`".to_string(),
    });
}

/// Best-effort kill of an agent working on a given task.
fn kill_agent_for_task(dir: &Path, task_id: &str) {
    let Ok(registry) = AgentRegistry::load(dir) else {
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
    use workgraph::graph::{Node, Task, TokenUsage, WaitCondition, WaitSpec, WorkGraph};
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
    fn test_reset_open_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Open)]);

        run(dir, "a", false, false, false).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        let task = graph.get_task("a").unwrap();
        assert_eq!(task.status, Status::Open);
    }

    #[test]
    fn test_reset_done_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut t = make_task("a", Status::Done);
        t.assigned = Some("agent-1".to_string());
        t.completed_at = Some("2025-01-01T00:00:00Z".to_string());
        t.retry_count = 3;
        t.session_id = Some("sess-1".to_string());
        setup(dir, vec![t]);

        run(dir, "a", false, false, false).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        let task = graph.get_task("a").unwrap();
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.assigned, None);
        assert_eq!(task.completed_at, None);
        assert_eq!(task.retry_count, 0);
        assert_eq!(task.session_id, None);
    }

    #[test]
    fn test_reset_failed_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut t = make_task("a", Status::Failed);
        t.failure_reason = Some("timeout".to_string());
        t.retry_count = 2;
        setup(dir, vec![t]);

        run(dir, "a", false, false, false).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        let task = graph.get_task("a").unwrap();
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.failure_reason, None);
        assert_eq!(task.retry_count, 0);
    }

    #[test]
    fn test_reset_abandoned_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Abandoned)]);

        run(dir, "a", false, false, false).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
    }

    #[test]
    fn test_reset_in_progress_task() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut t = make_task("a", Status::InProgress);
        t.assigned = Some("agent-1".to_string());
        t.started_at = Some("2025-01-01T00:00:00Z".to_string());
        setup(dir, vec![t]);

        run(dir, "a", false, false, false).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        let task = graph.get_task("a").unwrap();
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.assigned, None);
        assert_eq!(task.started_at, None);
    }

    #[test]
    fn test_reset_clears_all_execution_state() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut t = make_task("a", Status::Done);
        t.assigned = Some("agent-1".to_string());
        t.started_at = Some("2025-01-01T00:00:00Z".to_string());
        t.completed_at = Some("2025-01-01T01:00:00Z".to_string());
        t.failure_reason = Some("oops".to_string());
        t.retry_count = 5;
        t.loop_iteration = 3;
        t.cycle_failure_restarts = 2;
        t.session_id = Some("sess-1".to_string());
        t.checkpoint = Some("at step 3".to_string());
        t.wait_condition = Some(WaitSpec::Any(vec![WaitCondition::HumanInput]));
        t.token_usage = Some(TokenUsage {
            cost_usd: 1.5,
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        });
        t.paused = true;
        // Preserved fields
        t.description = Some("keep me".to_string());
        t.tags = vec!["important".to_string()];
        t.verify_cmd = Some("cargo test".to_string());
        t.model = Some("opus".to_string());
        t.artifacts = vec!["output.txt".to_string()];
        setup(dir, vec![t]);

        run(dir, "a", false, false, false).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        let task = graph.get_task("a").unwrap();
        // Cleared
        assert_eq!(task.status, Status::Open);
        assert_eq!(task.assigned, None);
        assert_eq!(task.started_at, None);
        assert_eq!(task.completed_at, None);
        assert_eq!(task.failure_reason, None);
        assert_eq!(task.retry_count, 0);
        assert_eq!(task.loop_iteration, 0);
        assert_eq!(task.cycle_failure_restarts, 0);
        assert_eq!(task.session_id, None);
        assert_eq!(task.checkpoint, None);
        assert_eq!(task.wait_condition, None);
        assert_eq!(task.token_usage, None);
        assert!(!task.paused);
        // Preserved
        assert_eq!(task.description.as_deref(), Some("keep me"));
        assert_eq!(task.tags, vec!["important"]);
        assert_eq!(task.verify_cmd.as_deref(), Some("cargo test"));
        assert_eq!(task.model.as_deref(), Some("opus"));
        assert_eq!(task.artifacts, vec!["output.txt"]);
    }

    #[test]
    fn test_reset_downstream() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();

        let mut a = make_task("a", Status::Done);
        a.completed_at = Some("2025-01-01T00:00:00Z".to_string());
        let mut b = make_task("b", Status::Done);
        b.after = vec!["a".to_string()];
        b.completed_at = Some("2025-01-01T01:00:00Z".to_string());
        let mut c = make_task("c", Status::Failed);
        c.after = vec!["b".to_string()];
        c.failure_reason = Some("err".to_string());

        setup(dir, vec![a, b, c]);

        run(dir, "a", true, false, false).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("a").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("b").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("c").unwrap().status, Status::Open);
        assert_eq!(graph.get_task("c").unwrap().failure_reason, None);
    }

    #[test]
    fn test_reset_dry_run() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut t = make_task("a", Status::Done);
        t.completed_at = Some("2025-01-01T00:00:00Z".to_string());
        setup(dir, vec![t]);

        run(dir, "a", false, false, true).unwrap();

        // Task should NOT be modified
        let graph = load_graph(&graph_path(dir)).unwrap();
        assert_eq!(graph.get_task("a").unwrap().status, Status::Done);
    }

    #[test]
    fn test_reset_adds_log_entry() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Failed)]);

        run(dir, "a", false, false, false).unwrap();

        let graph = load_graph(&graph_path(dir)).unwrap();
        let task = graph.get_task("a").unwrap();
        assert!(!task.log.is_empty());
        assert!(task.log.last().unwrap().message.contains("reset"));
    }

    #[test]
    fn test_reset_not_found() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Open)]);

        let err = run(dir, "nonexistent", false, false, false).unwrap_err();
        assert!(format!("{:#}", err).contains("not found"));
    }

    #[test]
    fn test_reset_records_provenance() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup(dir, vec![make_task("a", Status::Failed)]);

        run(dir, "a", false, false, false).unwrap();

        let entries = workgraph::provenance::read_all_operations(dir).unwrap();
        let reset_ops: Vec<_> = entries.iter().filter(|e| e.op == "reset").collect();
        assert_eq!(reset_ops.len(), 1);
        assert_eq!(reset_ops[0].task_id.as_deref(), Some("a"));
    }
}
