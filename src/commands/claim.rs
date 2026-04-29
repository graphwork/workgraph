use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::modify_graph;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::{load_graph, save_graph};

/// Claim a task for work: sets status to InProgress, optionally assigns an actor
pub fn claim(dir: &Path, id: &str, actor: Option<&str>) -> Result<()> {
    let path = super::graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut error: Option<anyhow::Error> = None;
    let mut prev_status = String::new();
    let mut prev_assigned: Option<String> = None;

    let _graph = modify_graph(&path, |graph| {
        let task = match graph.get_task_mut(id) {
            Some(t) => t,
            None => {
                error = Some(anyhow::anyhow!("Task '{}' not found", id));
                return false;
            }
        };

        // Only allow claiming tasks that are Open or Blocked
        match task.status {
            Status::Open | Status::Blocked | Status::Incomplete => {}
            Status::Waiting => {
                error = Some(anyhow::anyhow!(
                    "Cannot claim task '{}': task is Waiting. Use 'wg resume' first.",
                    id
                ));
                return false;
            }
            Status::InProgress => {
                let since = task
                    .started_at
                    .as_ref()
                    .map(|t| format!(" (since {})", t))
                    .unwrap_or_default();
                match &task.assigned {
                    Some(assigned) => {
                        error = Some(anyhow::anyhow!(
                            "Task '{}' is already claimed by @{}{}. Use 'wg unclaim {}' to release it first.",
                            id,
                            assigned,
                            since,
                            id
                        ));
                    }
                    None => {
                        error = Some(anyhow::anyhow!(
                            "Task '{}' is already in progress{}",
                            id,
                            since
                        ));
                    }
                }
                return false;
            }
            Status::Done => {
                error = Some(anyhow::anyhow!("Task '{}' is already done", id));
                return false;
            }
            Status::Failed => {
                error = Some(anyhow::anyhow!(
                    "Cannot claim task '{}': task is Failed. Use 'wg retry' to retry it.",
                    id
                ));
                return false;
            }
            Status::Abandoned => {
                error = Some(anyhow::anyhow!(
                    "Cannot claim task '{}': task is Abandoned",
                    id
                ));
                return false;
            }
            Status::PendingValidation => {
                error = Some(anyhow::anyhow!(
                    "Cannot claim task '{}': task is pending validation",
                    id
                ));
                return false;
            }
            Status::PendingEval => {
                error = Some(anyhow::anyhow!(
                    "Cannot claim task '{}': task is pending evaluation",
                    id
                ));
                return false;
            }
            Status::FailedPendingEval => {
                error = Some(anyhow::anyhow!(
                    "Cannot claim task '{}': task is pending rescue evaluation",
                    id
                ));
                return false;
            }
        }

        prev_status = format!("{:?}", task.status);
        prev_assigned = task.assigned.clone();

        task.status = Status::InProgress;
        task.started_at = Some(Utc::now().to_rfc3339());
        if let Some(actor_id) = actor {
            task.assigned = Some(actor_id.to_string());
        }

        let log_message = match actor {
            Some(actor_id) => format!("Task claimed by @{}", actor_id),
            None => "Task claimed".to_string(),
        };
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: actor.map(std::string::ToString::to_string),
            user: Some(workgraph::current_user()),
            message: log_message,
        });

        true
    })
    .context("Failed to save graph")?;

    if let Some(e) = error {
        return Err(e);
    }

    super::notify_graph_changed(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "claim",
        Some(id),
        actor,
        serde_json::json!({ "prev_status": prev_status, "prev_assigned": prev_assigned }),
        config.log.rotation_threshold,
    );

    match actor {
        Some(actor_id) => println!("Claimed '{}' for '{}'", id, actor_id),
        None => println!("Claimed '{}'", id),
    }

    Ok(())
}

/// Unclaim a task: sets status back to Open and clears assigned
pub fn unclaim(dir: &Path, id: &str) -> Result<()> {
    let path = super::graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut error: Option<anyhow::Error> = None;
    let mut prev_assigned: Option<String> = None;

    let _graph = modify_graph(&path, |graph| {
        let task = match graph.get_task_mut(id) {
            Some(t) => t,
            None => {
                error = Some(anyhow::anyhow!("Task '{}' not found", id));
                return false;
            }
        };

        // Only allow unclaiming tasks that are InProgress (or Open, as a no-op).
        // Terminal states should not be reverted via unclaim.
        match task.status {
            Status::InProgress | Status::Open | Status::Blocked | Status::Incomplete => {}
            Status::Waiting => {
                error = Some(anyhow::anyhow!(
                    "Cannot claim task '{}': task is Waiting. Use 'wg resume' first.",
                    id
                ));
                return false;
            }
            Status::Done => {
                error = Some(anyhow::anyhow!(
                    "Cannot unclaim task '{}': task is Done",
                    id
                ));
                return false;
            }
            Status::Failed => {
                error = Some(anyhow::anyhow!(
                    "Cannot unclaim task '{}': task is Failed",
                    id
                ));
                return false;
            }
            Status::Abandoned => {
                error = Some(anyhow::anyhow!(
                    "Cannot unclaim task '{}': task is Abandoned",
                    id
                ));
                return false;
            }
            Status::PendingValidation => {
                error = Some(anyhow::anyhow!(
                    "Cannot unclaim task '{}': task is pending validation",
                    id
                ));
                return false;
            }
            Status::PendingEval => {
                error = Some(anyhow::anyhow!(
                    "Cannot unclaim task '{}': task is pending evaluation",
                    id
                ));
                return false;
            }
            Status::FailedPendingEval => {
                error = Some(anyhow::anyhow!(
                    "Cannot unclaim task '{}': task is pending rescue evaluation",
                    id
                ));
                return false;
            }
        }

        prev_assigned = task.assigned.clone();
        task.status = Status::Open;
        task.assigned = None;

        let log_message = match &prev_assigned {
            Some(actor_id) => format!("Task unclaimed (was assigned to @{})", actor_id),
            None => "Task unclaimed".to_string(),
        };
        task.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: prev_assigned.clone(),
            user: Some(workgraph::current_user()),
            message: log_message,
        });

        true
    })
    .context("Failed to save graph")?;

    if let Some(e) = error {
        return Err(e);
    }

    // Kick: unclaiming may free a previously-blocked task for dispatch; user
    // expects immediate response, not a settling-delayed tick.
    super::notify_kick(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "unclaim",
        Some(id),
        prev_assigned.as_deref(),
        serde_json::json!({ "prev_assigned": prev_assigned }),
        config.log.rotation_threshold,
    );

    println!("Unclaimed '{}'", id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Task, WorkGraph};

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
    fn test_claim_open_task_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = claim(dir_path, "t1", None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
        assert!(task.assigned.is_none());
    }

    #[test]
    fn test_claim_with_actor() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        let result = claim(dir_path, "t1", Some("agent-1"));
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
        assert_eq!(task.assigned, Some("agent-1".to_string()));
    }

    #[test]
    fn test_claim_blocked_task_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::Blocked)],
        );

        let result = claim(dir_path, "t1", None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
    }

    #[test]
    fn test_claim_inprogress_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::InProgress)],
        );

        let result = claim(dir_path, "t1", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("already in progress"));
    }

    #[test]
    fn test_claim_done_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Done)]);

        let result = claim(dir_path, "t1", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("already done"));
    }

    #[test]
    fn test_claim_nonexistent_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![]);

        let result = claim(dir_path, "nonexistent", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_unclaim_inprogress_task_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = unclaim(dir_path, "t1");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
        assert!(task.assigned.is_none());
    }

    #[test]
    fn test_unclaim_open_task_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Open)]);

        // unclaim on an already open task should still work (idempotent)
        let result = unclaim(dir_path, "t1");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::Open);
    }

    #[test]
    fn test_unclaim_nonexistent_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![]);

        let result = unclaim(dir_path, "nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_claim_uninitialized_workgraph_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        // Don't initialize workgraph

        let result = claim(dir_path, "t1", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }

    #[test]
    fn test_unclaim_done_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Done)]);

        let result = unclaim(dir_path, "t1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Done"));
    }

    #[test]
    fn test_unclaim_failed_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Failed)]);

        let result = unclaim(dir_path, "t1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Failed"));
    }

    #[test]
    fn test_unclaim_abandoned_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::Abandoned)],
        );

        let result = unclaim(dir_path, "t1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Abandoned"));
    }

    #[test]
    fn test_claim_failed_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Test task", Status::Failed)]);

        let result = claim(dir_path, "t1", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Failed"));
        assert!(err.to_string().contains("retry"));
    }

    #[test]
    fn test_claim_abandoned_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::Abandoned)],
        );

        let result = claim(dir_path, "t1", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Abandoned"));
    }

    #[test]
    fn test_unclaim_uninitialized_workgraph_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        // Don't initialize workgraph

        let result = unclaim(dir_path, "t1");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }
}
