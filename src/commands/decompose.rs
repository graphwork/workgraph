use anyhow::Result;
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Node, Status, Task};

use super::add::generate_id;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(
    dir: &Path,
    parent_id: &str,
    subtask_titles: &[String],
    finalize_description: Option<&str>,
) -> Result<()> {
    if subtask_titles.is_empty() {
        anyhow::bail!("At least one --subtask is required");
    }

    // --- Autopoietic guardrails (pre-lock) ---
    let config = workgraph::config::Config::load_or_default(dir);
    let guardrails = config.guardrails.clone();

    let agent_id = std::env::var("WG_AGENT_ID").ok();
    if let Some(ref agent_id) = agent_id {
        let max_child = guardrails.max_child_tasks_per_agent;
        let count = super::add::count_agent_created_tasks(dir, agent_id);
        let remaining = max_child.saturating_sub(count);
        if subtask_titles.len() as u32 > remaining {
            anyhow::bail!(
                "Agent {} can only create {} more tasks ({}/{} used). \
                 Requested {} subtasks.",
                agent_id,
                remaining,
                count,
                max_child,
                subtask_titles.len()
            );
        }
    }

    let created_ids = super::mutate_workgraph(dir, |graph| {
        // Validate parent exists
        let parent = graph.get_task_or_err(parent_id)?;

        // Depth guardrail: subtasks will be at parent_depth + 1
        let parent_depth = graph.task_depth(parent_id);
        let subtask_depth = parent_depth + 1;
        if subtask_depth > guardrails.max_task_depth {
            anyhow::bail!(
                "Subtasks would be at depth {} (max: {}). \
                 Consider decomposing at a shallower level.",
                subtask_depth,
                guardrails.max_task_depth
            );
        }

        // Check parent is in a state that makes sense to decompose
        if parent.status == Status::Done {
            anyhow::bail!(
                "Cannot decompose '{}': task is already done",
                parent_id
            );
        }

        // Create all subtask nodes
        let mut created_ids = Vec::new();
        for title in subtask_titles {
            let subtask_id = generate_id(title, graph);

            let task = Task {
                id: subtask_id.clone(),
                title: title.clone(),
                status: Status::Open,
                created_at: Some(Utc::now().to_rfc3339()),
                visibility: "internal".to_string(),
                ..Task::default()
            };

            graph.add_node(Node::Task(task));
            created_ids.push(subtask_id);
        }

        // Add each subtask as a dependency of the parent (parent --after subtasks)
        let parent = graph.get_task_mut_or_err(parent_id)?;
        for sub_id in &created_ids {
            if !parent.after.contains(sub_id) {
                parent.after.push(sub_id.clone());
            }
        }

        // Transition parent: clear assignment, set to Open (not Blocked),
        // and add the decomposed tag
        parent.assigned = None;
        if parent.status == Status::InProgress {
            parent.status = Status::Open;
        }
        if !parent.tags.contains(&"decomposed".to_string()) {
            parent.tags.push("decomposed".to_string());
        }

        // Optionally update parent description with finalization guidance
        if let Some(fin_desc) = finalize_description {
            let current_desc = parent.description.clone().unwrap_or_default();
            parent.description = Some(format!(
                "{}\n\n## Finalization\n\n{}",
                current_desc, fin_desc
            ));
        }

        // Log the decomposition
        let subtask_list = created_ids.join(", ");
        parent.log.push(LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: agent_id.clone(),
            message: format!(
                "Decomposed into {} subtask(s): {}",
                created_ids.len(),
                subtask_list
            ),
            ..Default::default()
        });

        // Maintain bidirectional consistency: each subtask's `before` should include parent
        for sub_id in &created_ids {
            if let Some(subtask) = graph.get_task_mut(sub_id)
                && !subtask.before.contains(&parent_id.to_string())
            {
                subtask.before.push(parent_id.to_string());
            }
        }

        Ok(created_ids)
    })?;

    super::notify_graph_changed(dir);

    // Record provenance
    let mut detail = serde_json::json!({
        "parent_id": parent_id,
        "subtask_ids": created_ids,
    });
    if let Some(ref aid) = agent_id {
        detail["agent_id"] = serde_json::Value::String(aid.clone());
    }
    let _ = workgraph::provenance::record(
        dir,
        "decompose",
        Some(parent_id),
        agent_id.as_deref(),
        detail,
        config.log.rotation_threshold,
    );

    println!(
        "Decomposed '{}' into {} subtask(s): {}",
        parent_id,
        created_ids.len(),
        created_ids.join(", ")
    );
    println!(
        "Task '{}' is now blocked. Exit your session — the coordinator will re-dispatch when subtasks complete.",
        parent_id
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use workgraph::test_helpers::{make_task_with_status as make_task, setup_workgraph};

    #[test]
    fn test_decompose_creates_subtasks_and_blocks_parent() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("parent", "Big task", Status::InProgress)],
        );

        let result = run(
            dir_path,
            "parent",
            &["Part A".to_string(), "Part B".to_string()],
            None,
        );
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();

        // Parent should be Open (not InProgress) with subtasks in after
        let parent = graph.get_task("parent").unwrap();
        assert_eq!(parent.status, Status::Open);
        assert!(parent.assigned.is_none());
        assert!(parent.tags.contains(&"decomposed".to_string()));
        assert_eq!(parent.after.len(), 2);

        // Subtasks should exist
        let sub_a = graph.get_task(&parent.after[0]).unwrap();
        assert_eq!(sub_a.status, Status::Open);
        assert!(sub_a.before.contains(&"parent".to_string()));

        let sub_b = graph.get_task(&parent.after[1]).unwrap();
        assert_eq!(sub_b.status, Status::Open);
        assert!(sub_b.before.contains(&"parent".to_string()));
    }

    #[test]
    fn test_decompose_clears_assignment() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("parent", "Big task", Status::InProgress);
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, "parent", &["Sub 1".to_string()], None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let parent = graph.get_task("parent").unwrap();
        assert!(parent.assigned.is_none());
    }

    #[test]
    fn test_decompose_logs_event() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("parent", "Big task", Status::Open)],
        );

        let result = run(dir_path, "parent", &["Sub 1".to_string()], None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let parent = graph.get_task("parent").unwrap();

        let last_log = parent.log.last().unwrap();
        assert!(
            last_log.message.contains("Decomposed into 1 subtask(s)"),
            "got: {}",
            last_log.message
        );
    }

    #[test]
    fn test_decompose_with_finalize_description() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("parent", "Big task", Status::Open);
        task.description = Some("Original description".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(
            dir_path,
            "parent",
            &["Sub 1".to_string()],
            Some("Run cargo build and cargo test"),
        );
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let parent = graph.get_task("parent").unwrap();

        let desc = parent.description.as_ref().unwrap();
        assert!(desc.contains("Original description"));
        assert!(desc.contains("## Finalization"));
        assert!(desc.contains("Run cargo build and cargo test"));
    }

    #[test]
    fn test_decompose_no_subtasks_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("parent", "Big task", Status::Open)],
        );

        let result = run(dir_path, "parent", &[], None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("At least one"));
    }

    #[test]
    fn test_decompose_done_parent_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("parent", "Done task", Status::Done)],
        );

        let result = run(dir_path, "parent", &["Sub 1".to_string()], None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already done"));
    }

    #[test]
    fn test_decompose_nonexistent_parent_fails() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![]);

        let result = run(dir_path, "nonexistent", &["Sub 1".to_string()], None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_decompose_preserves_existing_after() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let existing_dep = make_task("existing-dep", "Existing dependency", Status::Open);
        let mut parent = make_task("parent", "Big task", Status::Open);
        parent.after = vec!["existing-dep".to_string()];
        setup_workgraph(dir_path, vec![existing_dep, parent]);

        let result = run(dir_path, "parent", &["New sub".to_string()], None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let parent = graph.get_task("parent").unwrap();

        // Should have both the existing dep AND the new subtask
        assert!(parent.after.contains(&"existing-dep".to_string()));
        assert_eq!(parent.after.len(), 2);
    }

    #[test]
    fn test_decompose_open_parent_stays_open() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("parent", "Open task", Status::Open)],
        );

        let result = run(dir_path, "parent", &["Sub 1".to_string()], None);
        assert!(result.is_ok());

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let parent = graph.get_task("parent").unwrap();
        assert_eq!(parent.status, Status::Open);
    }

    #[test]
    fn test_done_refuses_after_decompose() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("parent", "Big task", Status::InProgress)],
        );

        // Decompose the parent
        run(dir_path, "parent", &["Sub 1".to_string()], None).unwrap();

        // Try to mark done - should fail because subtask is unresolved
        let result = super::super::done::run(dir_path, "parent", false, false);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("blocked by") || err.contains("unresolved"),
            "Expected blocker error, got: {}",
            err
        );
    }
}
