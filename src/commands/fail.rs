use anyhow::Result;
use chrono::Utc;
use std::path::Path;
use workgraph::agency::capture_task_output;
use workgraph::graph::{LogEntry, Status, Task, evaluate_cycle_on_failure};

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(dir: &Path, id: &str, reason: Option<&str>) -> Result<()> {
    run_inner(dir, id, reason, false)
}

/// Reject a done task via evaluation gate. This allows failing a task that is
/// already Done — the evaluator determined the work is unacceptable.
pub fn run_eval_reject(dir: &Path, id: &str, reason: Option<&str>) -> Result<()> {
    run_inner(dir, id, reason, true)
}

struct FailResult {
    retry_count: u32,
    max_retries: Option<u32>,
    agent_id: Option<String>,
    cycle_reactivated: Vec<String>,
    task_snapshot: Option<Task>,
}

fn run_inner(dir: &Path, id: &str, reason: Option<&str>, eval_reject: bool) -> Result<()> {
    let result = super::mutate_workgraph(dir, |graph| {
        let task = graph.get_task_mut_or_err(id)?;

        if task.status == Status::Done && !eval_reject {
            anyhow::bail!(
                "Task '{}' is already done and cannot be marked as failed",
                id
            );
        }

        if task.status == Status::Abandoned {
            anyhow::bail!("Task '{}' is already abandoned", id);
        }

        if task.status == Status::Failed {
            println!(
                "Task '{}' is already failed (retry_count: {})",
                id, task.retry_count
            );
            return Ok(None);
        }

        task.status = Status::Failed;
        task.retry_count += 1;
        task.failure_reason = reason.map(String::from);

        let log_message = if eval_reject {
            match reason {
                Some(r) => format!("Evaluation rejected task: {}", r),
                None => "Evaluation rejected task".to_string(),
            }
        } else {
            match reason {
                Some(r) => format!("Task marked as failed: {}", r),
                None => "Task marked as failed".to_string(),
            }
        };
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: task.assigned.clone(),
            message: log_message,
            ..Default::default()
        });

        // Extract values we need before cycle restart may modify the task
        let retry_count = task.retry_count;
        let max_retries = task.max_retries;
        let agent_id = task.assigned.clone();

        // Evaluate cycle failure restart — if this task is part of a cycle with
        // restart_on_failure (default true), reset all cycle members to Open.
        let id_owned = id.to_string();
        let cycle_analysis = graph.compute_cycle_analysis();
        let cycle_reactivated = evaluate_cycle_on_failure(graph, &id_owned, &cycle_analysis);

        let task_snapshot = graph.get_task(id).cloned();

        Ok(Some(FailResult {
            retry_count,
            max_retries,
            agent_id,
            cycle_reactivated,
            task_snapshot,
        }))
    })?;

    let Some(result) = result else {
        return Ok(());
    };

    super::notify_graph_changed(dir);

    if !result.cycle_reactivated.is_empty() {
        println!(
            "  Cycle failure restart: re-activated {} task(s): {:?}",
            result.cycle_reactivated.len(),
            result.cycle_reactivated
        );
    }

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let detail = match reason {
        Some(r) => serde_json::json!({ "reason": r }),
        None => serde_json::Value::Null,
    };
    let _ = workgraph::provenance::record(
        dir,
        "fail",
        Some(id),
        None,
        detail,
        config.log.rotation_threshold,
    );

    let reason_msg = reason.map(|r| format!(" ({})", r)).unwrap_or_default();
    println!(
        "Marked '{}' as failed{} (retry #{})",
        id, reason_msg, result.retry_count
    );

    // Show retry info if max_retries is set
    if let Some(max) = result.max_retries {
        if result.retry_count >= max {
            println!(
                "  Warning: Max retries ({}) reached. Consider abandoning or increasing limit.",
                max
            );
        } else {
            println!("  Retries remaining: {}", max - result.retry_count);
        }
    }

    // Archive agent conversation (prompt + output) for provenance
    if let Some(ref agent_id) = result.agent_id {
        match super::log::archive_agent(dir, id, agent_id) {
            Ok(archive_dir) => {
                eprintln!("Agent archived to {}", archive_dir.display());
            }
            Err(e) => {
                eprintln!("Warning: agent archive failed: {}", e);
            }
        }
    }

    // Capture task output (git diff, artifacts, log) for evaluation.
    if let Some(ref task) = result.task_snapshot {
        match capture_task_output(dir, task) {
            Ok(output_dir) => {
                eprintln!("Output captured to {}", output_dir.display());
            }
            Err(e) => {
                eprintln!("Warning: output capture failed: {}", e);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use workgraph::test_helpers::{make_task_with_status as make_task, setup_workgraph};

    #[test]
    fn test_fail_in_progress_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", Some("compilation error"));
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
    }

    #[test]
    fn test_fail_open_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
    }

    #[test]
    fn test_fail_already_done_task_errors() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Done)]);

        let result = run(dir_path, "t1", Some("reason"));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("already done"),
            "Expected 'already done' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_fail_already_abandoned_task_errors() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::Abandoned)],
        );

        let result = run(dir_path, "t1", Some("reason"));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("already abandoned"),
            "Expected 'already abandoned' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_fail_increments_retry_count() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        run(dir_path, "t1", None).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.retry_count, 1);
    }

    #[test]
    fn test_fail_stores_failure_reason() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::InProgress)],
        );

        run(dir_path, "t1", Some("timeout exceeded")).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.failure_reason.as_deref(), Some("timeout exceeded"));
    }

    #[test]
    fn test_fail_no_reason_clears_failure_reason() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.failure_reason = Some("old reason".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", None).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.failure_reason, None);
    }

    #[test]
    fn test_fail_log_entry_includes_reason() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        run(dir_path, "t1", Some("network failure")).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(!task.log.is_empty());
        let last_log = task.log.last().unwrap();
        assert!(
            last_log.message.contains("network failure"),
            "Log message should contain reason, got: {}",
            last_log.message
        );
    }

    #[test]
    fn test_fail_log_entry_without_reason() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        run(dir_path, "t1", None).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        let last_log = task.log.last().unwrap();
        assert_eq!(last_log.message, "Task marked as failed");
    }

    #[test]
    fn test_fail_already_failed_is_noop() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 2;
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", Some("new reason"));
        assert!(result.is_ok());

        // Verify nothing changed
        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.retry_count, 2); // Unchanged
        assert_eq!(task.status, Status::Failed);
    }

    #[test]
    fn test_fail_task_not_found() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "nonexistent", None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_fail_captures_task_output() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        // Run fail - capture_task_output will be called but may fail in test env
        // (no git repo). The important thing is that run() itself still succeeds.
        let result = run(dir_path, "t1", None);
        assert!(result.is_ok());

        // Verify the task was still properly marked as failed despite capture outcome
        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
    }

    #[test]
    fn test_eval_reject_done_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Done)]);

        // Normal fail should error on done tasks
        let result = run(dir_path, "t1", Some("reason"));
        assert!(result.is_err());

        // eval_reject should succeed
        let result = run_eval_reject(
            dir_path,
            "t1",
            Some("evaluation score 0.3 below threshold 0.5"),
        );
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
        assert_eq!(task.retry_count, 1);
        assert!(
            task.failure_reason
                .as_deref()
                .unwrap()
                .contains("evaluation score")
        );
        // Check log message uses "Evaluation rejected" prefix
        let last_log = task.log.last().unwrap();
        assert!(last_log.message.contains("Evaluation rejected"));
    }

    #[test]
    fn test_eval_reject_already_failed_is_noop() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        setup_workgraph(dir_path, vec![task]);

        let result = run_eval_reject(dir_path, "t1", Some("reason"));
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.retry_count, 1); // Unchanged
    }
}
