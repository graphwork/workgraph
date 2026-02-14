use anyhow::{Context, Result};
use std::path::Path;
use workgraph::parser::load_graph;
use workgraph::query::cost_of;

use super::graph_path;

pub fn run(dir: &Path, id: &str) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    if graph.get_task(id).is_none() {
        anyhow::bail!("Task '{}' not found", id);
    }

    let total_cost = cost_of(&graph, id);

    println!(
        "Total cost for '{}' (including dependencies): ${:.2}",
        id, total_cost
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Estimate, Node, Status, Task, WorkGraph};
    use workgraph::parser::save_graph;

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
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
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
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
        let result = run(dir.path(), "t1");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[test]
    fn test_run_nonexistent_task() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![make_task("t1", "Task 1")]);
        let result = run(dir.path(), "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn test_run_single_task_with_cost() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Task 1");
        task.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(500.0),
        });
        setup_workgraph(dir.path(), vec![task]);

        let result = run(dir.path(), "t1");
        assert!(result.is_ok());
    }

    #[test]
    fn test_single_task_cost() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Task 1");
        task.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: Some(1000.0),
        });
        let path = setup_workgraph(dir.path(), vec![task]);

        let graph = load_graph(&path).unwrap();
        assert_eq!(cost_of(&graph, "t1"), 1000.0);
    }

    #[test]
    fn test_cost_aggregation_through_dependencies() {
        let dir = tempdir().unwrap();
        let mut dep1 = make_task("dep1", "Dep 1");
        dep1.estimate = Some(Estimate {
            hours: None,
            cost: Some(200.0),
        });

        let mut dep2 = make_task("dep2", "Dep 2");
        dep2.estimate = Some(Estimate {
            hours: None,
            cost: Some(300.0),
        });

        let mut main = make_task("main", "Main task");
        main.blocked_by = vec!["dep1".to_string(), "dep2".to_string()];
        main.estimate = Some(Estimate {
            hours: None,
            cost: Some(500.0),
        });

        let path = setup_workgraph(dir.path(), vec![dep1, dep2, main]);
        let graph = load_graph(&path).unwrap();
        // 500 + 200 + 300 = 1000
        assert_eq!(cost_of(&graph, "main"), 1000.0);
    }

    #[test]
    fn test_cost_transitive_dependencies() {
        let dir = tempdir().unwrap();
        let mut leaf = make_task("leaf", "Leaf");
        leaf.estimate = Some(Estimate {
            hours: None,
            cost: Some(100.0),
        });

        let mut mid = make_task("mid", "Middle");
        mid.blocked_by = vec!["leaf".to_string()];
        mid.estimate = Some(Estimate {
            hours: None,
            cost: Some(200.0),
        });

        let mut root = make_task("root", "Root");
        root.blocked_by = vec!["mid".to_string()];
        root.estimate = Some(Estimate {
            hours: None,
            cost: Some(300.0),
        });

        let path = setup_workgraph(dir.path(), vec![leaf, mid, root]);
        let graph = load_graph(&path).unwrap();
        // root(300) + mid(200) + leaf(100) = 600
        assert_eq!(cost_of(&graph, "root"), 600.0);
    }

    #[test]
    fn test_cost_missing_estimate() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "No estimate");
        let path = setup_workgraph(dir.path(), vec![task]);

        let graph = load_graph(&path).unwrap();
        assert_eq!(cost_of(&graph, "t1"), 0.0);
    }

    #[test]
    fn test_cost_missing_cost_field_in_estimate() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Hours only");
        task.estimate = Some(Estimate {
            hours: Some(10.0),
            cost: None,
        });
        let path = setup_workgraph(dir.path(), vec![task]);

        let graph = load_graph(&path).unwrap();
        // cost is None → defaults to 0.0
        assert_eq!(cost_of(&graph, "t1"), 0.0);
    }

    #[test]
    fn test_cost_dep_missing_estimate() {
        let dir = tempdir().unwrap();
        let dep = make_task("dep", "Dep with no estimate");
        let mut main = make_task("main", "Main");
        main.blocked_by = vec!["dep".to_string()];
        main.estimate = Some(Estimate {
            hours: None,
            cost: Some(500.0),
        });

        let path = setup_workgraph(dir.path(), vec![dep, main]);
        let graph = load_graph(&path).unwrap();
        // dep has no estimate (0.0), main has 500.0
        assert_eq!(cost_of(&graph, "main"), 500.0);
    }

    #[test]
    fn test_cost_nonexistent_task_returns_zero() {
        let dir = tempdir().unwrap();
        let path = setup_workgraph(dir.path(), vec![make_task("t1", "Task")]);

        let graph = load_graph(&path).unwrap();
        assert_eq!(cost_of(&graph, "nonexistent"), 0.0);
    }

    #[test]
    fn test_cost_handles_diamond_dependencies() {
        // Diamond: root -> mid1 -> leaf, root -> mid2 -> leaf
        // leaf should only be counted once
        let dir = tempdir().unwrap();
        let mut leaf = make_task("leaf", "Leaf");
        leaf.estimate = Some(Estimate {
            hours: None,
            cost: Some(100.0),
        });

        let mut mid1 = make_task("mid1", "Mid 1");
        mid1.blocked_by = vec!["leaf".to_string()];
        mid1.estimate = Some(Estimate {
            hours: None,
            cost: Some(200.0),
        });

        let mut mid2 = make_task("mid2", "Mid 2");
        mid2.blocked_by = vec!["leaf".to_string()];
        mid2.estimate = Some(Estimate {
            hours: None,
            cost: Some(300.0),
        });

        let mut root = make_task("root", "Root");
        root.blocked_by = vec!["mid1".to_string(), "mid2".to_string()];
        root.estimate = Some(Estimate {
            hours: None,
            cost: Some(400.0),
        });

        let path = setup_workgraph(dir.path(), vec![leaf, mid1, mid2, root]);
        let graph = load_graph(&path).unwrap();
        // root(400) + mid1(200) + mid2(300) + leaf(100) = 1000
        // leaf is only counted once due to visited set
        assert_eq!(cost_of(&graph, "root"), 1000.0);
    }

    #[test]
    fn test_cost_handles_cycle() {
        let dir = tempdir().unwrap();
        let mut t1 = make_task("t1", "Task 1");
        t1.blocked_by = vec!["t2".to_string()];
        t1.estimate = Some(Estimate {
            hours: None,
            cost: Some(100.0),
        });

        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];
        t2.estimate = Some(Estimate {
            hours: None,
            cost: Some(200.0),
        });

        let path = setup_workgraph(dir.path(), vec![t1, t2]);
        let graph = load_graph(&path).unwrap();
        // Should not infinite loop; each counted once: 100 + 200 = 300
        assert_eq!(cost_of(&graph, "t1"), 300.0);
    }

    #[test]
    fn test_run_task_no_cost() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![make_task("t1", "Task")]);

        let result = run(dir.path(), "t1");
        assert!(result.is_ok());
    }
}
