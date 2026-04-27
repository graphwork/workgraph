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

pub fn run(dir: &Path, id: &str, preserve_session: bool, fresh: bool) -> Result<()> {
    let path = super::graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // --fresh: discard the prior worktree (if any) so the next spawn allocates
    // a clean one off main. Default behavior is retry-in-place, which preserves
    // the existing worktree + branch so the next agent can resume WIP.
    let mut fresh_removed_path: Option<std::path::PathBuf> = None;
    if fresh {
        if let Some(project_root) = dir.parent() {
            if let Some((wt_path, branch)) =
                crate::commands::spawn::worktree::find_worktree_for_task(project_root, id)
            {
                eprintln!(
                    "[retry --fresh] Removing prior worktree for '{}' at {:?} (branch: {})",
                    id, wt_path, branch
                );
                let _ = crate::commands::spawn::worktree::remove_worktree(
                    project_root,
                    &wt_path,
                    &branch,
                );
                fresh_removed_path = Some(wt_path);
            }
        }
    } else {
        // Retry-in-place: clear any cleanup-pending marker so the dispatcher
        // tick doesn't reap the worktree before the next agent picks it up.
        if let Some(project_root) = dir.parent() {
            if let Some((wt_path, _)) =
                crate::commands::spawn::worktree::find_worktree_for_task(project_root, id)
            {
                let marker = wt_path.join(
                    crate::commands::service::worktree::CLEANUP_PENDING_MARKER,
                );
                if marker.exists() {
                    let _ = std::fs::remove_file(&marker);
                    eprintln!(
                        "[retry] Cleared cleanup-pending marker on prior worktree for '{}' (retry-in-place)",
                        id
                    );
                }
            }
        }
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

    if let Some(p) = fresh_removed_path {
        println!("  --fresh: discarded prior worktree at {:?}", p);
    } else if !fresh {
        // Inform the user that the next attempt will resume in-place if a
        // prior worktree exists.
        if let Some(project_root) = dir.parent() {
            if let Some((wt, _)) =
                crate::commands::spawn::worktree::find_worktree_for_task(project_root, id)
            {
                println!("  Next attempt will resume in-place at {:?}", wt);
            }
        }
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

        let result = run(dir_path, "t1", false, false);
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

        let result = run(dir_path, "t1", false, false);
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

        run(dir_path, "t1", false, false).unwrap();

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

        let result = run(dir_path, "t1", false, false);
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

        let result = run(dir_path, "t1", false, false);
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

        let result = run(dir_path, "t1", false, false);
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

        run(dir_path, "t1", false, false).unwrap();

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

        run(dir_path, "t1", false, false).unwrap();

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

        run(dir_path, "t1", false, false).unwrap();

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

        let result = run(dir_path, "t1", false, false);
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

        let result = run(dir_path, "t1", false, false);
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

        run(dir_path, "t1", false, false).unwrap();

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

        let result = run(dir_path, "nonexistent", false, false);
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

        run(dir_path, "t1", false, false).unwrap();

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

        run(dir_path, "t1", true, false).unwrap();

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

        run(dir_path, "t1", false, false).unwrap();

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

        run(dir_path, "t1", false, false).unwrap();

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

        run(dir_path, "t1", false, false).unwrap();

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

        run(dir_path, "t1", false, false).unwrap();

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

        run(dir_path, "t1", false, false).unwrap();

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

        run(dir_path, "t1", false, false).unwrap();

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

        run(dir_path, "t1", false, false).unwrap();

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

        run(dir_path, "t1", false, false).unwrap();

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(
            task.tier,
            Some("premium".to_string()),
            "Default tier (standard) should escalate to premium"
        );
    }

    /// Helper: init a git repo with a "main" branch and one commit.
    fn init_git_repo(path: &Path) {
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .arg(path)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["symbolic-ref", "HEAD", "refs/heads/main"])
            .current_dir(path)
            .output()
            .unwrap();
        fs::write(path.join("seed.txt"), "seed").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(path)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .unwrap();
    }

    /// Helper: create a worktree with the wg/<agent>/<task> branch convention.
    fn create_worktree(project: &Path, agent_id: &str, task_id: &str) -> std::path::PathBuf {
        let branch = format!("wg/{}/{}", agent_id, task_id);
        let wt = project.join(".wg-worktrees").join(agent_id);
        fs::create_dir_all(project.join(".wg-worktrees")).unwrap();
        std::process::Command::new("git")
            .args(["worktree", "add"])
            .arg(&wt)
            .args(["-b", &branch, "HEAD"])
            .current_dir(project)
            .output()
            .unwrap();
        wt
    }

    /// New retention policy (worktree-retention-don):
    /// `wg retry` (default) reuses the existing worktree + branch — does NOT
    /// remove the dir, does NOT delete the branch. Clears the cleanup-pending
    /// marker so the next sweep doesn't reap before the new agent picks up.
    #[test]
    fn test_retry_reuses_existing_worktree_by_default() {
        let temp = tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        let wg_dir = project.join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let mut task = make_task("retry-here", "test", Status::Failed);
        task.retry_count = 1;
        setup_workgraph(&wg_dir, vec![task]);

        // Pretend a prior agent ran in this worktree, made a commit, and
        // exited with a cleanup-pending marker.
        let wt = create_worktree(&project, "agent-prior", "retry-here");
        fs::write(wt.join("wip.txt"), "uncommitted-wip").unwrap();
        fs::write(
            wt.join(crate::commands::service::worktree::CLEANUP_PENDING_MARKER),
            "",
        )
        .unwrap();

        let result = run(&wg_dir, "retry-here", false, /*fresh=*/ false);
        assert!(result.is_ok(), "retry should succeed: {:?}", result);

        // Default behavior: worktree dir SURVIVES.
        assert!(wt.exists(), "retry must NOT remove worktree by default");
        assert!(
            wt.join("wip.txt").exists(),
            "uncommitted WIP must survive"
        );
        // Cleanup-pending marker should be cleared so the next sweep doesn't reap.
        assert!(
            !wt.join(crate::commands::service::worktree::CLEANUP_PENDING_MARKER)
                .exists(),
            "cleanup-pending marker must be cleared on retry-in-place"
        );
        // Branch survives in git
        let branches = std::process::Command::new("git")
            .args(["branch", "--list", "wg/agent-prior/retry-here"])
            .current_dir(&project)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).contains("wg/agent-prior/retry-here"),
            "branch must survive retry-in-place"
        );
    }

    /// `wg retry --fresh` discards the prior worktree + branch so the next
    /// spawn allocates a clean one off main.
    #[test]
    fn test_retry_fresh_flag_allocates_new_worktree() {
        let temp = tempdir().unwrap();
        let project = temp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        init_git_repo(&project);

        let wg_dir = project.join(".workgraph");
        fs::create_dir_all(&wg_dir).unwrap();

        let mut task = make_task("retry-fresh", "test", Status::Failed);
        task.retry_count = 1;
        setup_workgraph(&wg_dir, vec![task]);

        let wt = create_worktree(&project, "agent-prior", "retry-fresh");
        assert!(wt.exists());

        let result = run(&wg_dir, "retry-fresh", false, /*fresh=*/ true);
        assert!(result.is_ok(), "retry --fresh should succeed: {:?}", result);

        // --fresh: worktree dir is REMOVED.
        assert!(
            !wt.exists(),
            "retry --fresh must remove the prior worktree"
        );
        // Branch is also deleted
        let branches = std::process::Command::new("git")
            .args(["branch", "--list", "wg/agent-prior/retry-fresh"])
            .current_dir(&project)
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&branches.stdout).contains("wg/agent-prior/retry-fresh"),
            "branch must be deleted on --fresh"
        );
    }
}
