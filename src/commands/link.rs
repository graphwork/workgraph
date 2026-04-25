//! Link/unlink commands for managing task dependencies ergonomically.
//!
//! `wg link A B` — A depends on B (A comes after B)
//! `wg unlink A B` — removes the dependency from A to B

use anyhow::{Context, Result};
use std::path::Path;
use workgraph::graph::Status;
use workgraph::parser::modify_graph;

use super::graph_path;

/// Link: make `task_id` depend on `dependency_id` (task comes after dependency).
pub fn run_link(dir: &Path, task_id: &str, dependency_id: &str) -> Result<()> {
    if task_id == dependency_id {
        anyhow::bail!("A task cannot depend on itself");
    }

    let path = graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut error: Option<anyhow::Error> = None;
    let mut already_linked = false;
    let task_id_owned = task_id.to_string();
    let dep_id_owned = dependency_id.to_string();
    modify_graph(&path, |graph| {
        // Validate both tasks exist
        if graph.get_task(&task_id_owned).is_none() {
            error = Some(anyhow::anyhow!("Task '{}' not found", task_id_owned));
            return false;
        }
        if graph.get_task(&dep_id_owned).is_none() {
            error = Some(anyhow::anyhow!("Task '{}' not found", dep_id_owned));
            return false;
        }

        // Warn if the task is in-progress
        if graph
            .get_task(&task_id_owned)
            .is_some_and(|t| t.status == Status::InProgress)
        {
            eprintln!(
                "Warning: '{}' is currently in-progress. Adding a dependency on '{}' anyway.",
                task_id_owned, dep_id_owned
            );
        }

        // Add the forward edge: task.after includes dependency
        {
            let task = graph.get_task_mut(&task_id_owned).unwrap();
            if task.after.contains(&dep_id_owned) {
                already_linked = true;
                return false;
            }
            task.after.push(dep_id_owned.clone());
        }

        // Add the reverse edge: dependency.before includes task
        {
            let dep = graph.get_task_mut(&dep_id_owned).unwrap();
            if !dep.before.contains(&task_id_owned) {
                dep.before.push(task_id_owned.clone());
            }
        }

        true
    })
    .context("Failed to modify graph")?;
    if let Some(e) = error {
        return Err(e);
    }
    if already_linked {
        println!(
            "'{}' already depends on '{}' — no change",
            task_id, dependency_id
        );
        return Ok(());
    }
    super::notify_graph_changed(dir);

    // Record provenance
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "link",
        Some(task_id),
        None,
        serde_json::json!({
            "dependency": dependency_id,
            "action": "add",
        }),
        config.log.rotation_threshold,
    );

    println!("Linked: '{}' now depends on '{}'", task_id, dependency_id);
    Ok(())
}

/// Unlink: remove the dependency of `task_id` on `dependency_id`.
pub fn run_unlink(dir: &Path, task_id: &str, dependency_id: &str) -> Result<()> {
    let path = graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut error: Option<anyhow::Error> = None;
    let mut not_linked = false;
    let task_id_owned = task_id.to_string();
    let dep_id_owned = dependency_id.to_string();
    modify_graph(&path, |graph| {
        // Validate both tasks exist
        if graph.get_task(&task_id_owned).is_none() {
            error = Some(anyhow::anyhow!("Task '{}' not found", task_id_owned));
            return false;
        }
        if graph.get_task(&dep_id_owned).is_none() {
            error = Some(anyhow::anyhow!("Task '{}' not found", dep_id_owned));
            return false;
        }

        // Remove the forward edge
        let removed = {
            let task = graph.get_task_mut(&task_id_owned).unwrap();
            if let Some(pos) = task.after.iter().position(|x| x == &dep_id_owned) {
                task.after.remove(pos);
                true
            } else {
                false
            }
        };

        if !removed {
            not_linked = true;
            return false;
        }

        // Remove the reverse edge
        {
            let dep = graph.get_task_mut(&dep_id_owned).unwrap();
            dep.before.retain(|b| b != &task_id_owned);
        }

        true
    })
    .context("Failed to modify graph")?;
    if let Some(e) = error {
        return Err(e);
    }
    if not_linked {
        println!(
            "'{}' does not depend on '{}' — no change",
            task_id, dependency_id
        );
        return Ok(());
    }
    super::notify_graph_changed(dir);

    // Record provenance
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "unlink",
        Some(task_id),
        None,
        serde_json::json!({
            "dependency": dependency_id,
            "action": "remove",
        }),
        config.log.rotation_threshold,
    );

    println!(
        "Unlinked: '{}' no longer depends on '{}'",
        task_id, dependency_id
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use workgraph::parser::load_graph;

    fn setup_graph(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        let graph_path = graph_path(dir);
        std::fs::write(&graph_path, "").unwrap();

        crate::commands::add::run(
            dir,
            "Task A",
            Some("task-a"),
            None,
            &[],
            None,
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            None,
            None, // verify
            None, // verify_timeout
            None, // validation
            None, // validator_agent
            None, // validator_model
            None,
            None,
            None,
            false,
            false,
            None,
            "internal",
            None,
            None,
            None,
            None,
            false,
            false,
            &[],
            &[],
            None,
            None,
            false,
            false,
            false, // no_tier_escalation
            None,
            None,  // priority
            None,  // cron
            false, // subtask
        )
        .unwrap();

        crate::commands::add::run(
            dir,
            "Task B",
            Some("task-b"),
            None,
            &[],
            None,
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            None,
            None, // verify
            None, // verify_timeout
            None, // validation
            None, // validator_agent
            None, // validator_model
            None,
            None,
            None,
            false,
            false,
            None,
            "internal",
            None,
            None,
            None,
            None,
            false,
            false,
            &[],
            &[],
            None,
            None,
            false,
            false,
            false, // no_tier_escalation
            None,
            None,  // priority
            None,  // cron
            false, // subtask
        )
        .unwrap();

        crate::commands::add::run(
            dir,
            "Task C",
            Some("task-c"),
            None,
            &[],
            None,
            None,
            None,
            &[],
            &[],
            &[],
            &[],
            None,
            None,
            None,
            None, // verify
            None, // verify_timeout
            None, // validation
            None, // validator_agent
            None, // validator_model
            None,
            None,
            None,
            false,
            false,
            None,
            "internal",
            None,
            None,
            None,
            None,
            false,
            false,
            &[],
            &[],
            None,
            None,
            false,
            false,
            false, // no_tier_escalation
            None,
            None,  // priority
            None,  // cron
            false, // subtask
        )
        .unwrap();
    }

    #[test]
    fn link_creates_dependency() {
        let tmp = TempDir::new().unwrap();
        setup_graph(tmp.path());

        run_link(tmp.path(), "task-a", "task-b").unwrap();

        let graph = load_graph(graph_path(tmp.path())).unwrap();
        let a = graph.get_task("task-a").unwrap();
        assert!(a.after.contains(&"task-b".to_string()));

        let b = graph.get_task("task-b").unwrap();
        assert!(b.before.contains(&"task-a".to_string()));
    }

    #[test]
    fn link_idempotent() {
        let tmp = TempDir::new().unwrap();
        setup_graph(tmp.path());

        run_link(tmp.path(), "task-a", "task-b").unwrap();
        run_link(tmp.path(), "task-a", "task-b").unwrap(); // no-op

        let graph = load_graph(graph_path(tmp.path())).unwrap();
        let a = graph.get_task("task-a").unwrap();
        assert_eq!(
            a.after.iter().filter(|x| *x == "task-b").count(),
            1,
            "should not duplicate"
        );
    }

    #[test]
    fn link_self_rejected() {
        let tmp = TempDir::new().unwrap();
        setup_graph(tmp.path());

        let result = run_link(tmp.path(), "task-a", "task-a");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("cannot depend on itself")
        );
    }

    #[test]
    fn link_nonexistent_task_rejected() {
        let tmp = TempDir::new().unwrap();
        setup_graph(tmp.path());

        let result = run_link(tmp.path(), "task-a", "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn unlink_removes_dependency() {
        let tmp = TempDir::new().unwrap();
        setup_graph(tmp.path());

        run_link(tmp.path(), "task-a", "task-b").unwrap();
        run_unlink(tmp.path(), "task-a", "task-b").unwrap();

        let graph = load_graph(graph_path(tmp.path())).unwrap();
        let a = graph.get_task("task-a").unwrap();
        assert!(!a.after.contains(&"task-b".to_string()));

        let b = graph.get_task("task-b").unwrap();
        assert!(!b.before.contains(&"task-a".to_string()));
    }

    #[test]
    fn unlink_nonexistent_edge_is_noop() {
        let tmp = TempDir::new().unwrap();
        setup_graph(tmp.path());

        // No dependency exists — should succeed without error
        let result = run_unlink(tmp.path(), "task-a", "task-b");
        assert!(result.is_ok());
    }

    #[test]
    fn link_multiple_dependencies() {
        let tmp = TempDir::new().unwrap();
        setup_graph(tmp.path());

        run_link(tmp.path(), "task-a", "task-b").unwrap();
        run_link(tmp.path(), "task-a", "task-c").unwrap();

        let graph = load_graph(graph_path(tmp.path())).unwrap();
        let a = graph.get_task("task-a").unwrap();
        assert!(a.after.contains(&"task-b".to_string()));
        assert!(a.after.contains(&"task-c".to_string()));
    }

    #[test]
    fn unlink_one_keeps_other() {
        let tmp = TempDir::new().unwrap();
        setup_graph(tmp.path());

        run_link(tmp.path(), "task-a", "task-b").unwrap();
        run_link(tmp.path(), "task-a", "task-c").unwrap();
        run_unlink(tmp.path(), "task-a", "task-b").unwrap();

        let graph = load_graph(graph_path(tmp.path())).unwrap();
        let a = graph.get_task("task-a").unwrap();
        assert!(!a.after.contains(&"task-b".to_string()));
        assert!(a.after.contains(&"task-c".to_string()));
    }
}
