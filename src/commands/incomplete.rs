use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status, parse_token_usage, parse_wg_tokens};
use workgraph::parser::modify_graph;
use workgraph::service::registry::AgentRegistry;

pub fn run(dir: &Path, id: &str, reason: Option<&str>) -> Result<()> {
    {
        let (graph, _path) = super::load_workgraph_mut(dir)?;
        let task = graph.get_task_or_err(id)?;

        if task.status == Status::Incomplete {
            println!("Task '{}' is already incomplete", id);
            return Ok(());
        }

        if task.status.is_terminal() {
            anyhow::bail!(
                "Task '{}' is {} and cannot be marked as incomplete",
                id,
                task.status
            );
        }
    }

    let path = super::graph_path(dir);

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

    let config = workgraph::config::Config::load_or_default(dir);
    let max_incomplete_retries = config.coordinator.max_incomplete_retries;
    let escalate_on_retry = config.coordinator.escalate_on_retry;
    let retry_delay = &config.coordinator.incomplete_retry_delay;

    let ready_after = if !retry_delay.is_empty() && retry_delay != "0s" && retry_delay != "0" {
        parse_delay_to_rfc3339(retry_delay).ok()
    } else {
        None
    };

    let mut agent_id_for_archive = None;
    let mut final_status = Status::Incomplete;
    let mut final_retry_count: u32 = 0;

    let id_owned = id.to_string();
    let reason_owned = reason.map(String::from);
    let ready_after_owned = ready_after.clone();
    modify_graph(&path, |graph| {
        let task = match graph.get_task_mut(&id_owned) {
            Some(t) => t,
            None => return false,
        };

        if task.status == Status::Incomplete || task.status.is_terminal() {
            return false;
        }

        agent_id_for_archive = task.assigned.clone();

        task.retry_count += 1;
        final_retry_count = task.retry_count;

        // Determine effective max retries: task-level overrides global config
        let effective_max = task.max_retries.unwrap_or(max_incomplete_retries);

        if effective_max > 0 && task.retry_count >= effective_max {
            // Exhausted retries — transition to Failed
            task.status = Status::Failed;
            task.failure_reason = Some(format!(
                "Retry exhausted ({}/{} attempts). Last incomplete reason: {}",
                task.retry_count,
                effective_max,
                reason_owned.as_deref().unwrap_or("unspecified")
            ));
            final_status = Status::Failed;

            let log_message = format!(
                "Retry exhausted after {} attempts — task failed. Last reason: {}",
                task.retry_count,
                reason_owned.as_deref().unwrap_or("unspecified")
            );
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: agent_id_for_archive.clone(),
                user: Some(workgraph::current_user()),
                message: log_message,
            });
        } else {
            // Still has retries remaining — mark incomplete for re-dispatch
            task.status = Status::Incomplete;
            final_status = Status::Incomplete;

            if let Some(ref ra) = ready_after_owned {
                task.ready_after = Some(ra.clone());
            }

            // Tier escalation on retry: bump fast→standard→premium
            if escalate_on_retry && !task.no_tier_escalation {
                use workgraph::config::Tier;
                let current_tier: Tier = task
                    .tier
                    .as_deref()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(Tier::Standard);
                let next_tier = current_tier.escalate();
                if next_tier != current_tier {
                    task.tier = Some(next_tier.to_string());
                    task.log.push(LogEntry {
                        timestamp: Utc::now().to_rfc3339(),
                        actor: agent_id_for_archive.clone(),
                        user: Some(workgraph::current_user()),
                        message: format!(
                            "Tier escalated on retry: {} → {}",
                            current_tier, next_tier
                        ),
                    });
                }
            }

            let remaining = if effective_max > 0 {
                format!(" ({} remaining)", effective_max - task.retry_count)
            } else {
                String::new()
            };

            let log_message = match reason_owned.as_deref() {
                Some(r) => format!(
                    "Task marked as incomplete (attempt #{}{}): {}",
                    task.retry_count, remaining, r
                ),
                None => format!(
                    "Task marked as incomplete (attempt #{}{})",
                    task.retry_count, remaining
                ),
            };
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: agent_id_for_archive.clone(),
                user: Some(workgraph::current_user()),
                message: log_message,
            });
        }

        task.assigned = None;
        task.completed_at = Some(Utc::now().to_rfc3339());
        task.session_id = None;
        task.checkpoint = None;

        if task.token_usage.is_none()
            && let Some(ref usage) = token_usage
        {
            task.token_usage = Some(usage.clone());
        }

        true
    })
    .context("Failed to save graph")?;

    super::notify_graph_changed(dir);

    if let Ok(mut locked_registry) = AgentRegistry::load_locked(dir) {
        if let Some(agent) = locked_registry.get_agent_by_task_mut(id) {
            use workgraph::service::registry::AgentStatus;
            agent.status = AgentStatus::Done;
            if agent.completed_at.is_none() {
                agent.completed_at = Some(Utc::now().to_rfc3339());
            }
        }
        let _ = locked_registry.save_ref();
    }

    let detail = match reason {
        Some(r) => serde_json::json!({
            "reason": r,
            "retry_count": final_retry_count,
            "final_status": final_status.to_string(),
        }),
        None => serde_json::json!({
            "retry_count": final_retry_count,
            "final_status": final_status.to_string(),
        }),
    };
    let _ = workgraph::provenance::record(
        dir,
        "incomplete",
        Some(id),
        None,
        detail,
        config.log.rotation_threshold,
    );

    match final_status {
        Status::Failed => {
            let reason_msg = reason.map(|r| format!(" ({})", r)).unwrap_or_default();
            println!(
                "Task '{}' failed — retry exhausted after {} attempts{}",
                id, final_retry_count, reason_msg
            );
        }
        _ => {
            let effective_max = {
                let (graph, _) = super::load_workgraph_mut(dir)?;
                let task = graph.get_task_or_err(id)?;
                task.max_retries.unwrap_or(max_incomplete_retries)
            };
            let reason_msg = reason.map(|r| format!(" ({})", r)).unwrap_or_default();
            println!(
                "Marked '{}' as incomplete{} (attempt {}/{})",
                id,
                reason_msg,
                final_retry_count,
                if effective_max > 0 {
                    effective_max.to_string()
                } else {
                    "∞".to_string()
                }
            );
            if let Some(ref ra) = ready_after {
                println!("  Cooldown active — dispatchable after {}", ra);
            }
            println!("  Task will appear in 'wg ready' for re-dispatch");
        }
    }

    if let Some(ref agent_id) = agent_id_for_archive {
        match super::log::archive_agent(dir, id, agent_id) {
            Ok(archive_dir) => {
                eprintln!("Agent archived to {}", archive_dir.display());
            }
            Err(e) => {
                eprintln!("Warning: failed to archive agent: {}", e);
            }
        }
    }

    Ok(())
}

fn parse_delay_to_rfc3339(delay_str: &str) -> Result<String> {
    let delay_str = delay_str.trim();
    if delay_str.is_empty() {
        anyhow::bail!("Empty delay string");
    }

    let (num_str, unit) = if let Some(s) = delay_str.strip_suffix('s') {
        (s, "s")
    } else if let Some(s) = delay_str.strip_suffix('m') {
        (s, "m")
    } else if let Some(s) = delay_str.strip_suffix('h') {
        (s, "h")
    } else {
        (delay_str, "s")
    };

    let num: u64 = num_str.parse().context("Invalid delay number")?;
    let secs = match unit {
        "m" => num * 60,
        "h" => num * 3600,
        _ => num,
    };

    let future = Utc::now() + chrono::Duration::seconds(secs as i64);
    Ok(future.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::{load_graph, save_graph};

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
        let path = dir.join("graph.jsonl");
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
        path
    }

    fn setup_config(dir: &Path, max_retries: u32, delay: &str) {
        let config_path = dir.join("config.toml");
        let content = format!(
            "[coordinator]\nmax_incomplete_retries = {}\nincomplete_retry_delay = \"{}\"",
            max_retries, delay
        );
        fs::write(config_path, content).unwrap();
    }

    #[test]
    fn test_incomplete_increments_retry_count() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let task = make_task("t1", "Test task", Status::InProgress);
        setup_workgraph(dir_path, vec![task]);
        setup_config(dir_path, 3, "0s");

        run(dir_path, "t1", Some("needs more work")).unwrap();

        let path = dir_path.join("graph.jsonl");
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Incomplete);
        assert_eq!(task.retry_count, 1);
    }

    #[test]
    fn test_incomplete_exhaustion_transitions_to_failed() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.retry_count = 2; // Already 2 retries
        setup_workgraph(dir_path, vec![task]);
        setup_config(dir_path, 3, "0s");

        run(dir_path, "t1", Some("still broken")).unwrap();

        let path = dir_path.join("graph.jsonl");
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Failed);
        assert!(
            task.failure_reason
                .as_ref()
                .unwrap()
                .contains("Retry exhausted")
        );
        assert_eq!(task.retry_count, 3);
    }

    #[test]
    fn test_incomplete_task_level_max_retries_overrides_config() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.retry_count = 4;
        task.max_retries = Some(5);
        setup_workgraph(dir_path, vec![task]);
        setup_config(dir_path, 3, "0s");

        run(dir_path, "t1", None).unwrap();

        let path = dir_path.join("graph.jsonl");
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        // task max_retries=5, retry_count was 4, now 5 — should exhaust
        assert_eq!(task.status, Status::Failed);
        assert_eq!(task.retry_count, 5);
    }

    #[test]
    fn test_incomplete_clears_assigned_and_session() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.assigned = Some("agent-42".to_string());
        task.session_id = Some("sess-123".to_string());
        task.checkpoint = Some("checkpoint data".to_string());
        setup_workgraph(dir_path, vec![task]);
        setup_config(dir_path, 3, "0s");

        run(dir_path, "t1", None).unwrap();

        let path = dir_path.join("graph.jsonl");
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.assigned, None);
        assert_eq!(task.session_id, None);
        assert_eq!(task.checkpoint, None);
    }

    #[test]
    fn test_incomplete_cooldown_sets_ready_after() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let task = make_task("t1", "Test task", Status::InProgress);
        setup_workgraph(dir_path, vec![task]);
        setup_config(dir_path, 3, "30s");

        run(dir_path, "t1", None).unwrap();

        let path = dir_path.join("graph.jsonl");
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(
            task.ready_after.is_some(),
            "Should have ready_after set for cooldown"
        );
    }

    #[test]
    fn test_incomplete_zero_delay_no_ready_after() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let task = make_task("t1", "Test task", Status::InProgress);
        setup_workgraph(dir_path, vec![task]);
        setup_config(dir_path, 3, "0s");

        run(dir_path, "t1", None).unwrap();

        let path = dir_path.join("graph.jsonl");
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.ready_after, None);
    }

    #[test]
    fn test_incomplete_zero_max_retries_means_unlimited() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.retry_count = 100;
        setup_workgraph(dir_path, vec![task]);
        setup_config(dir_path, 0, "0s");

        run(dir_path, "t1", None).unwrap();

        let path = dir_path.join("graph.jsonl");
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Incomplete);
        assert_eq!(task.retry_count, 101);
    }

    #[test]
    fn test_incomplete_log_entry_includes_attempt_number() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.retry_count = 1;
        setup_workgraph(dir_path, vec![task]);
        setup_config(dir_path, 5, "0s");

        run(dir_path, "t1", Some("missing tests")).unwrap();

        let path = dir_path.join("graph.jsonl");
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        let last = task.log.last().unwrap();
        assert!(
            last.message.contains("attempt #2"),
            "Log should mention attempt #2, got: {}",
            last.message
        );
        assert!(
            last.message.contains("missing tests"),
            "Log should contain reason"
        );
    }

    #[test]
    fn test_incomplete_already_incomplete_is_noop() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let task = make_task("t1", "Test task", Status::Incomplete);
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", None);
        assert!(result.is_ok());
    }

    #[test]
    fn test_incomplete_terminal_task_errors() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let task = make_task("t1", "Test task", Status::Done);
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "t1", None);
        assert!(result.is_err());
    }
}
