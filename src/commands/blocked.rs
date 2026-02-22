use anyhow::Result;
use std::path::Path;
use workgraph::query::after;

#[cfg(test)]
use super::graph_path;
#[cfg(test)]
use workgraph::parser::load_graph;

pub fn run(dir: &Path, id: &str, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    if graph.get_task(id).is_none() {
        anyhow::bail!("Task '{}' not found", id);
    }

    let blockers = after(&graph, id);

    if json {
        let output: Vec<_> = blockers
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "title": t.title,
                    "status": t.status,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if blockers.is_empty() {
        println!("Task '{}' has no blockers", id);
    } else {
        println!("Task '{}' is blocked by:", id);
        for blocker in blockers {
            println!(
                "  {} - {} [{:?}]",
                blocker.id, blocker.title, blocker.status
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Status, Task, WorkGraph};
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
    fn test_run_uninitialized() {
        let dir = tempdir().unwrap();
        let result = run(dir.path(), "t1", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[test]
    fn test_run_nonexistent_task() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![make_task("t1", "Task 1", Status::Open)]);
        let result = run(dir.path(), "nonexistent", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_run_task_with_blockers() {
        let dir = tempdir().unwrap();
        let blocker = make_task("blocker", "Blocker task", Status::Open);
        let mut blocked = make_task("blocked", "Blocked task", Status::Open);
        blocked.after = vec!["blocker".to_string()];
        setup_workgraph(dir.path(), vec![blocker, blocked]);

        let result = run(dir.path(), "blocked", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_unblocked_task() {
        let dir = tempdir().unwrap();
        setup_workgraph(
            dir.path(),
            vec![make_task("t1", "No blockers", Status::Open)],
        );
        let result = run(dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_blocker_query_returns_correct_blockers() {
        let dir = tempdir().unwrap();
        let b1 = make_task("b1", "Blocker 1", Status::Open);
        let b2 = make_task("b2", "Blocker 2", Status::InProgress);
        let mut task = make_task("t1", "Blocked task", Status::Open);
        task.after = vec!["b1".to_string(), "b2".to_string()];
        let path = setup_workgraph(dir.path(), vec![b1, b2, task]);

        let graph = load_graph(&path).unwrap();
        let blockers = after(&graph, "t1");
        assert_eq!(blockers.len(), 2);

        let ids: Vec<&str> = blockers.iter().map(|b| b.id.as_str()).collect();
        assert!(ids.contains(&"b1"));
        assert!(ids.contains(&"b2"));
    }

    #[test]
    fn test_blocker_query_excludes_done_blockers() {
        let dir = tempdir().unwrap();
        let b1 = make_task("b1", "Done blocker", Status::Done);
        let b2 = make_task("b2", "Open blocker", Status::Open);
        let mut task = make_task("t1", "Task", Status::Open);
        task.after = vec!["b1".to_string(), "b2".to_string()];
        let path = setup_workgraph(dir.path(), vec![b1, b2, task]);

        let graph = load_graph(&path).unwrap();
        let blockers = after(&graph, "t1");
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].id, "b2");
    }

    #[test]
    fn test_blocker_query_empty_for_unblocked_task() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "Free task", Status::Open);
        let path = setup_workgraph(dir.path(), vec![task]);

        let graph = load_graph(&path).unwrap();
        let blockers = after(&graph, "t1");
        assert!(blockers.is_empty());
    }

    #[test]
    fn test_blocker_query_empty_when_all_done() {
        let dir = tempdir().unwrap();
        let b1 = make_task("b1", "Done 1", Status::Done);
        let b2 = make_task("b2", "Done 2", Status::Done);
        let mut task = make_task("t1", "Task", Status::Open);
        task.after = vec!["b1".to_string(), "b2".to_string()];
        let path = setup_workgraph(dir.path(), vec![b1, b2, task]);

        let graph = load_graph(&path).unwrap();
        let blockers = after(&graph, "t1");
        assert!(blockers.is_empty());
    }

    #[test]
    fn test_run_json_output_with_blockers() {
        let dir = tempdir().unwrap();
        let blocker = make_task("b1", "Blocker", Status::Open);
        let mut task = make_task("t1", "Blocked", Status::Open);
        task.after = vec!["b1".to_string()];
        setup_workgraph(dir.path(), vec![blocker, task]);

        let result = run(dir.path(), "t1", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_json_output_structure() {
        let dir = tempdir().unwrap();
        let blocker = make_task("b1", "Blocker task", Status::InProgress);
        let mut task = make_task("t1", "Blocked", Status::Open);
        task.after = vec!["b1".to_string()];
        let path = setup_workgraph(dir.path(), vec![blocker, task]);

        let graph = load_graph(&path).unwrap();
        let blockers = after(&graph, "t1");
        assert_eq!(blockers.len(), 1);

        let json = serde_json::json!({
            "id": blockers[0].id,
            "title": blockers[0].title,
            "status": blockers[0].status,
        });
        assert_eq!(json["id"], "b1");
        assert_eq!(json["title"], "Blocker task");
        assert_eq!(json["status"], "in-progress");
    }

    #[test]
    fn test_run_json_empty_blockers() {
        let dir = tempdir().unwrap();
        setup_workgraph(
            dir.path(),
            vec![make_task("t1", "No blockers", Status::Open)],
        );
        let result = run(dir.path(), "t1", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_nonexistent_task_json() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![make_task("t1", "Task", Status::Open)]);
        let result = run(dir.path(), "nonexistent", true);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }
}
