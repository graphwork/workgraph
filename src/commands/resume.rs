use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::LogEntry;
use workgraph::parser::save_graph;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(dir: &Path, id: &str) -> Result<()> {
    run_inner(dir, id, false)
}

/// Publish a draft task (alias for resume with validation messaging).
pub fn publish(dir: &Path, id: &str) -> Result<()> {
    run_inner(dir, id, true)
}

fn run_inner(dir: &Path, id: &str, is_publish: bool) -> Result<()> {
    let (mut graph, path) = super::load_workgraph_mut(dir)?;

    let task = graph.get_task_mut_or_err(id)?;

    if !task.paused {
        anyhow::bail!("Task '{}' is not paused", id);
    }

    // Validate all --after dependencies exist before resuming/publishing
    let after_deps = task.after.clone();
    let mut missing_deps = Vec::new();
    for dep_id in &after_deps {
        if workgraph::federation::parse_remote_ref(dep_id).is_some() {
            continue; // Cross-repo deps validated at resolution time
        }
        if graph.get_node(dep_id).is_none() {
            let mut msg = format!("'{}'", dep_id);
            // Suggest fuzzy match
            let all_ids: Vec<&str> = graph.tasks().map(|t| t.id.as_str()).collect();
            if let Some((suggestion, _)) =
                workgraph::check::fuzzy_match_task_id(dep_id, all_ids.iter().copied(), 3)
            {
                msg.push_str(&format!(" (did you mean '{}'?)", suggestion));
            }
            missing_deps.push(msg);
        }
    }

    if !missing_deps.is_empty() {
        anyhow::bail!(
            "Cannot {} task '{}': dangling dependencies:\n  {}",
            if is_publish { "publish" } else { "resume" },
            id,
            missing_deps.join("\n  ")
        );
    }

    let task = graph.get_task_mut_or_err(id)?;
    task.paused = false;
    let action = if is_publish { "published" } else { "resumed" };
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: None,
        message: format!("Task {}", action),
    });

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let op = if is_publish { "publish" } else { "resume" };
    let _ = workgraph::provenance::record(
        dir,
        op,
        Some(id),
        None,
        serde_json::json!({}),
        config.log.rotation_threshold,
    );

    if is_publish {
        println!("Published '{}' — task is now available for dispatch", id);
    } else {
        println!("Resumed '{}'", id);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Status, Task, WorkGraph};

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
    fn test_resume_paused_task() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        setup_workgraph(dir.path(), vec![task]);

        let result = run(dir.path(), "t1");
        assert!(result.is_ok());

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(!task.paused);
    }

    #[test]
    fn test_resume_not_paused_fails() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![make_task("t1", "Test", Status::Open)]);

        let result = run(dir.path(), "t1");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not paused"));
    }

    #[test]
    fn test_resume_nonexistent_task_fails() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![]);

        let result = run(dir.path(), "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_resume_adds_log_entry() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        setup_workgraph(dir.path(), vec![task]);

        run(dir.path(), "t1").unwrap();

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert_eq!(task.log.len(), 1);
        assert!(task.log[0].message.contains("resumed"));
    }

    #[test]
    fn test_resume_with_dangling_dep_fails() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        task.after = vec!["nonexistent-dep".to_string()];
        setup_workgraph(dir.path(), vec![task]);

        let result = run(dir.path(), "t1");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("dangling dependencies"), "got: {msg}");
        assert!(msg.contains("nonexistent-dep"), "got: {msg}");
    }

    #[test]
    fn test_resume_with_valid_deps_succeeds() {
        let dir = tempdir().unwrap();
        let dep = make_task("dep1", "Dependency", Status::Open);
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        task.after = vec!["dep1".to_string()];
        setup_workgraph(dir.path(), vec![dep, task]);

        let result = run(dir.path(), "t1");
        assert!(result.is_ok());

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(!task.paused);
    }

    #[test]
    fn test_publish_with_dangling_dep_fails() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        task.after = vec!["missing-task".to_string()];
        setup_workgraph(dir.path(), vec![task]);

        let result = publish(dir.path(), "t1");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Cannot publish"), "got: {msg}");
        assert!(msg.contains("dangling dependencies"), "got: {msg}");
    }

    #[test]
    fn test_publish_with_valid_deps_succeeds() {
        let dir = tempdir().unwrap();
        let dep = make_task("dep1", "Dependency", Status::Open);
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        task.after = vec!["dep1".to_string()];
        setup_workgraph(dir.path(), vec![dep, task]);

        let result = publish(dir.path(), "t1");
        assert!(result.is_ok());

        let graph = load_graph(graph_path(dir.path())).unwrap();
        let task = graph.get_task("t1").unwrap();
        assert!(!task.paused);
        assert!(task.log.last().unwrap().message.contains("published"));
    }

    #[test]
    fn test_publish_no_deps_succeeds() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        setup_workgraph(dir.path(), vec![task]);

        let result = publish(dir.path(), "t1");
        assert!(result.is_ok());
    }

    #[test]
    fn test_resume_with_multiple_dangling_deps_lists_all() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Test", Status::Open);
        task.paused = true;
        task.after = vec!["missing-a".to_string(), "missing-b".to_string()];
        setup_workgraph(dir.path(), vec![task]);

        let result = run(dir.path(), "t1");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("missing-a"), "got: {msg}");
        assert!(msg.contains("missing-b"), "got: {msg}");
    }
}
