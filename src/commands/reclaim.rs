use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

/// Reclaim a task from a dead/unresponsive agent
///
/// This allows forcefully taking over a task that is currently assigned to another agent.
/// The task must be in InProgress status to be reclaimed.
pub fn run(dir: &Path, task_id: &str, from_actor: &str, to_actor: &str) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    // Verify the target actor exists
    if graph.get_actor(to_actor).is_none() {
        anyhow::bail!("Target actor '{}' not found", to_actor);
    }

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Check that task is in progress
    if task.status != Status::InProgress {
        anyhow::bail!(
            "Task '{}' is not in progress (status: {:?}). Only in-progress tasks can be reclaimed.",
            task_id,
            task.status
        );
    }

    // Check that task is assigned to the specified actor
    match &task.assigned {
        Some(assigned) if assigned == from_actor => {
            // Good - can proceed with reclaim
        }
        Some(assigned) => {
            anyhow::bail!(
                "Task '{}' is assigned to '{}', not '{}'. Cannot reclaim.",
                task_id,
                assigned,
                from_actor
            );
        }
        None => {
            anyhow::bail!(
                "Task '{}' has no assigned actor. Use 'wg claim' instead.",
                task_id
            );
        }
    }

    // Perform the reclaim
    let now = Utc::now().to_rfc3339();
    task.assigned = Some(to_actor.to_string());

    // Log the reclaim event
    let log_message = format!(
        "Task reclaimed from @{} to @{} (agent takeover)",
        from_actor, to_actor
    );
    task.log.push(LogEntry {
        timestamp: now.clone(),
        actor: Some(to_actor.to_string()),
        message: log_message.clone(),
    });

    save_graph(&graph, &path).context("Failed to save graph")?;

    println!(
        "Reclaimed task '{}' from '{}' to '{}'",
        task_id, from_actor, to_actor
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Actor, ActorType, Node, Task, TrustLevel, WorkGraph};

    fn make_task(id: &str, title: &str, status: Status) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status,
            assigned: None,
            estimate: None,
            blocks: vec![],
            blocked_by: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
        }
    }

    fn make_actor(id: &str) -> Actor {
        Actor {
            id: id.to_string(),
            name: None,
            role: Some("agent".to_string()),
            rate: None,
            capacity: None,
            capabilities: vec![],
            context_limit: None,
            trust_level: TrustLevel::Provisional,
            last_seen: None,
            actor_type: ActorType::Agent,
            matrix_user_id: None,
            response_times: vec![],
        }
    }

    fn setup_workgraph(dir: &Path, tasks: Vec<Task>, actors: Vec<Actor>) -> std::path::PathBuf {
        fs::create_dir_all(dir).unwrap();
        let path = graph_path(dir);
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        for actor in actors {
            graph.add_node(Node::Actor(actor));
        }
        save_graph(&graph, &path).unwrap();
        path
    }

    #[test]
    fn test_reclaim_inprogress_task_succeeds() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.assigned = Some("agent-old".to_string());

        setup_workgraph(
            dir_path,
            vec![task],
            vec![make_actor("agent-old"), make_actor("agent-new")],
        );

        let result = run(dir_path, "t1", "agent-old", "agent-new");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.status, Status::InProgress);
        assert_eq!(task.assigned, Some("agent-new".to_string()));
        assert!(!task.log.is_empty());
        assert!(task.log.last().unwrap().message.contains("reclaimed"));
    }

    #[test]
    fn test_reclaim_wrong_from_actor_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.assigned = Some("agent-actual".to_string());

        setup_workgraph(
            dir_path,
            vec![task],
            vec![
                make_actor("agent-actual"),
                make_actor("agent-old"),
                make_actor("agent-new"),
            ],
        );

        let result = run(dir_path, "t1", "agent-old", "agent-new");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("assigned to 'agent-actual'"));
    }

    #[test]
    fn test_reclaim_open_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::Open)],
            vec![make_actor("agent-old"), make_actor("agent-new")],
        );

        let result = run(dir_path, "t1", "agent-old", "agent-new");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not in progress"));
    }

    #[test]
    fn test_reclaim_unassigned_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        setup_workgraph(
            dir_path,
            vec![make_task("t1", "Test task", Status::InProgress)],
            vec![make_actor("agent-old"), make_actor("agent-new")],
        );

        let result = run(dir_path, "t1", "agent-old", "agent-new");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("no assigned actor"));
    }

    #[test]
    fn test_reclaim_nonexistent_task_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        setup_workgraph(
            dir_path,
            vec![],
            vec![make_actor("agent-old"), make_actor("agent-new")],
        );

        let result = run(dir_path, "nonexistent", "agent-old", "agent-new");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_reclaim_nonexistent_target_actor_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.assigned = Some("agent-old".to_string());

        setup_workgraph(dir_path, vec![task], vec![make_actor("agent-old")]);

        let result = run(dir_path, "t1", "agent-old", "agent-nonexistent");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Target actor"));
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn test_reclaim_uninitialized_workgraph_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        // Don't initialize workgraph

        let result = run(dir_path, "t1", "agent-old", "agent-new");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("not initialized"));
    }

    #[test]
    fn test_reclaim_log_entry_has_correct_actor() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "Test task", Status::InProgress);
        task.assigned = Some("agent-old".to_string());

        setup_workgraph(
            dir_path,
            vec![task],
            vec![make_actor("agent-old"), make_actor("agent-new")],
        );

        let result = run(dir_path, "t1", "agent-old", "agent-new");
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let task = graph.get_task("t1").unwrap();

        let log_entry = task.log.last().unwrap();
        assert_eq!(log_entry.actor, Some("agent-new".to_string()));
        assert!(log_entry.message.contains("agent-old"));
        assert!(log_entry.message.contains("agent-new"));
    }
}
