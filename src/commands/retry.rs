use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::config::Tier;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::modify_graph;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(dir: &Path, id: &str, preserve_session: bool) -> Result<()> {
    let path = super::graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let config = workgraph::config::Config::load_or_default(dir);
    let escalate_on_retry = config.coordinator.escalate_on_retry;

    let mut error: Option<anyhow::Error> = None;
    let mut prev_failure_reason: Option<String> = None;
    let mut attempt: u32 = 0;
    let mut retry_count: u32 = 0;
    let mut max_retries: Option<u32> = None;
    let mut was_incomplete = false;
    let mut tier_escalation_msg: Option<String> = None;

    modify_graph(&path, |graph| {
        let task = match graph.get_task_mut(id) {
            Some(t) => t,
            None => {
                error = Some(anyhow::anyhow!("Task '{}' not found", id));
                return false;
            }
        };

        if task.status != Status::Failed && task.status != Status::Incomplete {
            error = Some(anyhow::anyhow!(
                "Task '{}' is not failed or incomplete (status: {:?}). Only failed or incomplete tasks can be retried.",
                id,
                task.status
            ));
            return false;
        }

        was_incomplete = task.status == Status::Incomplete;

        // Check if max retries exceeded (for failed tasks)
        if task.status == Status::Failed
            && let Some(max) = task.max_retries
            && task.retry_count >= max
        {
            error = Some(anyhow::anyhow!(
                "Task '{}' has reached max retries ({}/{}). Consider abandoning or increasing max_retries.",
                id,
                task.retry_count,
                max
            ));
            return false;
        }

        prev_failure_reason = task.failure_reason.clone();
        attempt = task.retry_count + 1;

        // Clear failure state and set to Open status
        task.status = Status::Open;
        task.failure_reason = None;
        task.assigned = None;
        task.ready_after = None;
        if !preserve_session {
            task.session_id = None;
            task.checkpoint = None;
        }
        task.tags.retain(|t| t != "converged");

        // Tier escalation on retry: bump fast→standard→premium
        if escalate_on_retry && !task.no_tier_escalation {
            let current_tier: Tier = task
                .tier
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(Tier::Standard);
            let next_tier = current_tier.escalate();
            if next_tier != current_tier {
                task.tier = Some(next_tier.to_string());
                let msg = format!("Tier escalated on retry: {} → {}", current_tier, next_tier);
                task.log.push(LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: None,
                    user: Some(workgraph::current_user()),
                    message: msg.clone(),
                });
                tier_escalation_msg = Some(msg);
            }
        }

        let source = if was_incomplete {
            "incomplete"
        } else {
            "failed"
        };
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            user: Some(workgraph::current_user()),
            message: format!(
                "Task reset for retry from {} (attempt #{})",
                source,
                task.retry_count + 1
            ),
        });

        retry_count = task.retry_count;
        max_retries = task.max_retries;

        true
    })
    .context("Failed to modify graph")?;

    if let Some(e) = error {
        return Err(e);
    }

    // Set task status to Open after retry (dependency checking is done by ready/service logic)
    modify_graph(&path, |graph| {
        let task = graph.get_task_mut(id).unwrap(); // We know it exists from above
        task.status = Status::Open;
        true
    })
    .context("Failed to update task status after retry")?;

    super::notify_graph_changed(dir);

    // Record operation
    let _ = workgraph::provenance::record(
        dir,
        "retry",
        Some(id),
        None,
        serde_json::json!({
            "attempt": attempt,
            "prev_failure_reason": prev_failure_reason,
            "was_incomplete": was_incomplete,
            "tier_escalation": tier_escalation_msg,
        }),
        config.log.rotation_threshold,
    );

    let source = if was_incomplete {
        "incomplete"
    } else {
        "failed"
    };
    println!(
        "Reset '{}' from {} to open for retry (attempt #{})",
        id,
        source,
        retry_count + 1
    );

    if let Some(max) = max_retries {
        println!("  Retries remaining after this: {}", max - retry_count);
    }

    if let Some(ref msg) = tier_escalation_msg {
        println!("  {}", msg);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..Task::default()
        }
    }

    fn setup_workgraph(dir: &Path, tasks: Vec<Task>) -> std::path::PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
        path
    }

    #[test]
    fn test_retry_failed_task_transitions_to_open() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.failure_reason = Some("timeout".to_string());
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
    }

    #[test]
    fn test_retry_incomplete_task_transitions_to_open() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Incomplete);
        task.retry_count = 1;
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
    }

    #[test]
    fn test_retry_incomplete_clears_ready_after() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Incomplete);
        task.retry_count = 1;
        task.ready_after = Some("2099-01-01T00:00:00Z".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.ready_after, None,
            "Retry should clear ready_after cooldown"
        );
    }

    #[test]
    fn test_retry_non_failed_task_errors_open() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = run(dir_path, "t1", false);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not failed or incomplete"),
            "Expected error about status, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_retry_non_failed_task_errors_in_progress() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::InProgress)],
        );

        let result = run(dir_path, "t1", false);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not failed or incomplete")
        );
    }

    #[test]
    fn test_retry_non_failed_task_errors_done() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Done)]);

        let result = run(dir_path, "t1", false);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not failed or incomplete")
        );
    }

    #[test]
    fn test_retry_preserves_retry_count() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 3;
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.retry_count, 3,
            "retry_count should be preserved, not reset"
        );
    }

    #[test]
    fn test_retry_clears_failure_reason() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.failure_reason = Some("compilation error".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.failure_reason, None);
    }

    #[test]
    fn test_retry_clears_assigned() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.assigned, None);
    }

    #[test]
    fn test_retry_max_retries_exceeded() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 3;
        task.max_retries = Some(3);
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("max retries"),
            "Expected 'max retries' error, got: {}",
            err_msg
        );
    }

    #[test]
    fn test_retry_within_max_retries_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.max_retries = Some(3);
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", false);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
    }

    #[test]
    fn test_retry_adds_log_entry() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 2;
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(!task.log.is_empty());
        let last_log = task.log.last().unwrap();
        assert!(
            last_log.message.contains("retry"),
            "Log message should mention retry, got: {}",
            last_log.message
        );
        assert!(
            last_log.message.contains("3"),
            "Log message should contain attempt number 3, got: {}",
            last_log.message
        );
    }

    #[test]
    fn test_retry_task_not_found() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Failed)]);

        let result = run(dir_path, "nonexistent", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_retry_clears_session_id() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.session_id = Some("fce3a8ba-549c-440d-882d-dbfd5d2b371a".to_string());
        task.checkpoint = Some("Previous checkpoint context".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.session_id, None,
            "Retry should clear session_id to avoid --resume with dead session"
        );
        assert_eq!(
            task.checkpoint, None,
            "Retry should clear checkpoint along with session_id"
        );
    }

    #[test]
    fn test_retry_preserve_session_keeps_session_id() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.session_id = Some("keep-me-alive".to_string());
        task.checkpoint = Some("checkpoint content".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", true).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.session_id,
            Some("keep-me-alive".to_string()),
            "--preserve-session should keep session_id"
        );
        assert_eq!(
            task.checkpoint,
            Some("checkpoint content".to_string()),
            "--preserve-session should keep checkpoint"
        );
    }

    #[test]
    fn test_retry_clears_converged_tag() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.tags.push("converged".to_string());
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(
            !task.tags.contains(&"converged".to_string()),
            "Retry should clear converged tag"
        );
    }

    #[test]
    fn test_retry_incomplete_log_mentions_incomplete() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Incomplete);
        task.retry_count = 1;
        setup_workgraph(dir_path, vec![task]);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        let last_log = task.log.last().unwrap();
        assert!(
            last_log.message.contains("incomplete"),
            "Log should mention source was incomplete, got: {}",
            last_log.message
        );
    }

    fn setup_config_with_escalation(dir: &Path) {
        let config_path = dir.join("config.toml");
        fs::write(
            config_path,
            "[coordinator]\nescalate_on_retry = true\n",
        )
        .unwrap();
    }

    #[test]
    fn test_retry_escalates_tier_standard_to_premium() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.tier = Some("standard".to_string());
        setup_workgraph(dir_path, vec![task]);
        setup_config_with_escalation(dir_path);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.tier, Some("premium".to_string()));
        assert!(
            task.log.iter().any(|e| e.message.contains("Tier escalated")),
            "Should log tier escalation"
        );
    }

    #[test]
    fn test_retry_escalates_tier_fast_to_standard() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.tier = Some("fast".to_string());
        setup_workgraph(dir_path, vec![task]);
        setup_config_with_escalation(dir_path);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.tier, Some("standard".to_string()));
    }

    #[test]
    fn test_retry_caps_at_premium() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.tier = Some("premium".to_string());
        setup_workgraph(dir_path, vec![task]);
        setup_config_with_escalation(dir_path);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.tier,
            Some("premium".to_string()),
            "Premium should not escalate further"
        );
        assert!(
            !task.log.iter().any(|e| e.message.contains("Tier escalated")),
            "No escalation log when already at premium"
        );
    }

    #[test]
    fn test_retry_no_escalation_without_config() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.tier = Some("fast".to_string());
        setup_workgraph(dir_path, vec![task]);
        // No escalation config — default is false

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.tier,
            Some("fast".to_string()),
            "Should not escalate when config is off"
        );
    }

    #[test]
    fn test_retry_no_escalation_with_opt_out() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        task.tier = Some("fast".to_string());
        task.no_tier_escalation = true;
        setup_workgraph(dir_path, vec![task]);
        setup_config_with_escalation(dir_path);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.tier,
            Some("fast".to_string()),
            "Should not escalate when no_tier_escalation is set"
        );
    }

    #[test]
    fn test_retry_default_tier_escalates() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::Failed);
        task.retry_count = 1;
        // No tier set — defaults to Standard
        setup_workgraph(dir_path, vec![task]);
        setup_config_with_escalation(dir_path);

        run(dir_path, "t1", false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.tier,
            Some("premium".to_string()),
            "Default tier (standard) should escalate to premium"
        );
    }
}
