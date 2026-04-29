use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::agency::capture_task_output;
use workgraph::config::Config;
use workgraph::graph::{
    FailureClass, LogEntry, Status, evaluate_cycle_on_failure, parse_token_usage, parse_wg_tokens,
};
use workgraph::parser::modify_graph;
use workgraph::service::registry::AgentRegistry;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(dir: &Path, id: &str, reason: Option<&str>, class: Option<FailureClass>) -> Result<()> {
    run_inner(dir, id, reason, class, false)
}

/// Reject a done task via evaluation gate. This allows failing a task that is
/// already Done — the evaluator determined the work is unacceptable.
pub fn run_eval_reject(dir: &Path, id: &str, reason: Option<&str>) -> Result<()> {
    run_inner(dir, id, reason, None, true)
}

fn run_inner(
    dir: &Path,
    id: &str,
    reason: Option<&str>,
    class: Option<FailureClass>,
    eval_reject: bool,
) -> Result<()> {
    // Pre-check with a non-atomic read (gate only — not used for mutation).
    {
        let (graph, _path) = super::load_workgraph_mut(dir)?;
        let task = graph.get_task_or_err(id)?;

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
            return Ok(());
        }

        // PendingEval is the new soft-done state: eval-gated rejection from
        // this state is the primary path. External `wg fail` is also allowed
        // (no special-case needed — the generic "anything non-terminal can be
        // failed" branch below covers it).
    }

    let path = super::graph_path(dir);

    // If this is an AgentExitNonzero failure AND auto_evaluate is on, route to
    // FailedPendingEval instead of terminal Failed so the evaluator can rescue
    // the task when the output is actually acceptable.
    let use_failed_pending_eval = !eval_reject
        && class == Some(FailureClass::AgentExitNonzero)
        && Config::load_or_default(dir).agency.auto_evaluate;

    // Resolve token usage outside the lock (registry read + file I/O).
    let token_usage = AgentRegistry::load(dir).ok().and_then(|registry| {
        let agent = registry.get_agent_by_task(id)?;
        let output_path = std::path::Path::new(&agent.output_file);
        let abs_path = if output_path.is_absolute() {
            output_path.to_path_buf()
        } else {
            dir.parent().unwrap_or(dir).join(output_path)
        };
        parse_token_usage(&abs_path).or_else(|| parse_wg_tokens(&abs_path))
    });

    // Atomically load the freshest graph, apply the mutation, and save.
    // Using modify_graph prevents lost updates from concurrent graph writers.
    let mut retry_count = 0u32;
    let mut max_retries = None;
    let mut agent_id_for_archive = None;
    let mut cycle_reactivated = Vec::new();
    let mut already_failed = false;

    let id_owned = id.to_string();
    let reason_owned = reason.map(String::from);
    let graph = modify_graph(&path, |graph| {
        let task = match graph.get_task_mut(&id_owned) {
            Some(t) => t,
            None => return false,
        };

        // Re-check status under lock
        if task.status == Status::Failed {
            already_failed = true;
            retry_count = task.retry_count;
            return false;
        }
        if task.status == Status::Abandoned {
            return false;
        }
        if task.status == Status::Done && !eval_reject {
            return false;
        }
        // PendingEval → Failed is allowed from both `wg fail` and the
        // eval-reject path. Falls through to the generic mutation below.
        //
        // FailedPendingEval → Failed is the terminal path after eval rejection
        // (or operator-forced fail). Does NOT trigger auto-rescue spawn.

        // Route to FailedPendingEval when conditions are met (Fork 5):
        // agent-exit-nonzero + auto_evaluate + not an eval-reject call.
        // Do NOT re-enter FailedPendingEval if already there (operator can
        // force terminal-fail from FailedPendingEval by calling wg fail again).
        if use_failed_pending_eval && task.status != Status::FailedPendingEval {
            task.status = Status::FailedPendingEval;
            task.failure_class = class;
            task.failure_reason = reason_owned.clone();
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: task.assigned.clone(),
                user: Some(workgraph::current_user()),
                message: "Agent exited without wg done — entering failed-pending-eval for rescue evaluation".to_string(),
            });

            // Apply pre-resolved token usage
            if task.token_usage.is_none()
                && let Some(ref usage) = token_usage
            {
                task.token_usage = Some(usage.clone());
            }

            retry_count = task.retry_count;
            max_retries = task.max_retries;
            agent_id_for_archive = task.assigned.clone();
            return true;
        }

        task.status = Status::Failed;
        task.retry_count += 1;
        task.failure_reason = reason_owned.clone();
        task.failure_class = class;

        let log_message = if eval_reject {
            match reason_owned.as_deref() {
                Some(r) => format!("Evaluation rejected task: {}", r),
                None => "Evaluation rejected task".to_string(),
            }
        } else {
            match reason_owned.as_deref() {
                Some(r) => format!("Task marked as failed: {}", r),
                None => "Task marked as failed".to_string(),
            }
        };
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: task.assigned.clone(),
            user: Some(workgraph::current_user()),
            message: log_message,
        });

        // Apply pre-resolved token usage
        if task.token_usage.is_none()
            && let Some(ref usage) = token_usage
        {
            task.token_usage = Some(usage.clone());
        }

        // Extract values we need before cycle restart may modify the task
        retry_count = task.retry_count;
        max_retries = task.max_retries;
        agent_id_for_archive = task.assigned.clone();

        // Evaluate cycle failure restart — if this task is part of a cycle with
        // restart_on_failure (default true), reset all cycle members to Open.
        let cycle_analysis = graph.compute_cycle_analysis();
        cycle_reactivated = evaluate_cycle_on_failure(graph, &id_owned, &cycle_analysis);

        true
    })
    .context("Failed to save graph")?;

    if already_failed {
        println!(
            "Task '{}' is already failed (retry_count: {})",
            id, retry_count
        );
        return Ok(());
    }

    super::notify_graph_changed(dir);

    // Update agent registry to reflect task failure.
    // Without this, the registry entry stays at Working until the daemon's
    // periodic triage detects the dead process.
    if let Ok(mut locked_registry) = AgentRegistry::load_locked(dir) {
        if let Some(agent) = locked_registry.get_agent_by_task_mut(id) {
            use workgraph::service::registry::AgentStatus;
            agent.status = AgentStatus::Failed;
            if agent.completed_at.is_none() {
                agent.completed_at = Some(Utc::now().to_rfc3339());
            }
        }
        let _ = locked_registry.save_ref();
    }

    if !cycle_reactivated.is_empty() {
        println!(
            "  Cycle failure restart: re-activated {} task(s): {:?}",
            cycle_reactivated.len(),
            cycle_reactivated
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
        id, reason_msg, retry_count
    );

    // Show retry info if max_retries is set
    if let Some(max) = max_retries {
        if retry_count >= max {
            println!(
                "  Warning: Max retries ({}) reached. Consider abandoning or increasing limit.",
                max
            );
        } else {
            println!("  Retries remaining: {}", max - retry_count);
        }
    }

    // Archive agent conversation (prompt + output) for provenance
    // Use agent_id captured before cycle restart (which clears assigned)
    if let Some(ref agent_id) = agent_id_for_archive {
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
    // Failed tasks are also evaluated when auto_evaluate is enabled — there is
    // useful signal in what kinds of tasks cause which agents to fail.
    if let Some(task) = graph.get_task(id) {
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

        let result = run(dir_path, "t1", Some("compilation error"), None);
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

        let result = run(dir_path, "t1", None, None);
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

        let result = run(dir_path, "t1", Some("reason"), None);
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

        let result = run(dir_path, "t1", Some("reason"), None);
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

        run(dir_path, "t1", None, None).unwrap();

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

        run(dir_path, "t1", Some("timeout exceeded"), None).unwrap();

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

        run(dir_path, "t1", None, None).unwrap();

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

        run(dir_path, "t1", Some("network failure"), None).unwrap();

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

        run(dir_path, "t1", None, None).unwrap();

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

        let result = run(dir_path, "t1", Some("new reason"), None);
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

        let result = run(dir_path, "nonexistent", None, None);
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
        let result = run(dir_path, "t1", None, None);
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
        let result = run(dir_path, "t1", Some("reason"), None);
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

    #[test]
    fn test_fail_updates_agent_registry() {
        // When a task is marked failed, the agent registry entry should also
        // transition to Failed so the agent slot is freed immediately.
        use workgraph::service::registry::{AgentRegistry, AgentStatus};

        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        // Set up a registry with an agent working on this task
        let mut registry = AgentRegistry::new();
        registry.register_agent(99999, "t1", "claude", "/tmp/output.log");
        registry.save(dir_path).unwrap();

        let result = run(dir_path, "t1", Some("test failure"), None);
        assert!(result.is_ok());

        // Verify registry was updated
        let registry = AgentRegistry::load(dir_path).unwrap();
        let agent = registry.get_agent("agent-1").unwrap();
        assert_eq!(
            agent.status,
            AgentStatus::Failed,
            "Agent registry should be updated to Failed when task fails"
        );
        assert!(
            agent.completed_at.is_some(),
            "Agent should have a completed_at timestamp"
        );
    }
}
