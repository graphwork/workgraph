use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;
use workgraph::graph::Status;
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

/// Auto-generated task prefixes that should be gc'd alongside their parent task.
const INTERNAL_PREFIXES: &[&str] = &["assign-", "evaluate-"];

pub fn run(dir: &Path, dry_run: bool, include_done: bool) -> Result<()> {
    let path = graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    // Collect all task IDs and their statuses for dependency checking
    let all_tasks: Vec<_> = graph.tasks().cloned().collect();

    // Build a set of task IDs that have non-terminal dependents.
    // A task should NOT be gc'd if any task that lists it in blocked_by is non-terminal.
    let mut has_open_dependent: HashSet<String> = HashSet::new();
    for task in &all_tasks {
        if !task.status.is_terminal() {
            // This task is non-terminal — all its blockers are "needed"
            for blocker_id in &task.blocked_by {
                has_open_dependent.insert(blocker_id.clone());
            }
        }
    }

    // Find tasks eligible for gc
    let mut to_gc: HashSet<String> = HashSet::new();
    for task in &all_tasks {
        if !task.status.is_terminal() {
            continue;
        }
        // By default, only gc failed + abandoned. With --include-done, also gc done.
        if task.status == Status::Done && !include_done {
            continue;
        }
        // Safety: skip if any non-terminal task depends on this one
        if has_open_dependent.contains(&task.id) {
            continue;
        }
        to_gc.insert(task.id.clone());
    }

    // Also collect internal tasks (assign-*, evaluate-*) whose parent is being gc'd
    for task in &all_tasks {
        for prefix in INTERNAL_PREFIXES {
            if let Some(parent_id) = task.id.strip_prefix(prefix)
                && to_gc.contains(parent_id)
                && task.status.is_terminal()
            {
                to_gc.insert(task.id.clone());
            }
        }
    }

    // Also gc internal tasks that are themselves terminal with no open dependents,
    // even if their parent was already removed (e.g., parent was archived but
    // internal tasks were left behind)
    for task in &all_tasks {
        if to_gc.contains(&task.id) {
            continue;
        }
        let is_internal = INTERNAL_PREFIXES
            .iter()
            .any(|prefix| task.id.starts_with(prefix));
        if is_internal && task.status.is_terminal() && !has_open_dependent.contains(&task.id) {
            to_gc.insert(task.id.clone());
        }
    }

    if to_gc.is_empty() {
        println!("No tasks to garbage collect.");
        return Ok(());
    }

    // Sort for deterministic output
    let mut gc_list: Vec<_> = to_gc.iter().cloned().collect();
    gc_list.sort();

    if dry_run {
        println!("Would remove {} tasks:", gc_list.len());
        for id in &gc_list {
            if let Some(task) = graph.get_task(id) {
                println!("  {} - {} [{}]", task.id, task.title, task.status);
            }
        }
        return Ok(());
    }

    // Capture details of tasks being removed for provenance
    let removed_details: Vec<serde_json::Value> = gc_list
        .iter()
        .filter_map(|id| {
            graph.get_task(id).map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "status": format!("{:?}", t.status),
                    "title": t.title,
                })
            })
        })
        .collect();

    let mut modified_graph = graph;
    for id in &gc_list {
        modified_graph.remove_node(id);
    }

    save_graph(&modified_graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    // Record operation
    let config = workgraph::config::Config::load_or_default(dir);
    let _ = workgraph::provenance::record(
        dir,
        "gc",
        None,
        None,
        serde_json::json!({ "removed": removed_details }),
        config.log.rotation_threshold,
    );

    println!("Removed {} tasks:", gc_list.len());
    for id in &gc_list {
        println!("  {}", id);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use workgraph::graph::{Node, WorkGraph};

    fn make_task(id: &str, title: &str, status: Status) -> workgraph::graph::Task {
        workgraph::graph::Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..workgraph::graph::Task::default()
        }
    }

    fn make_task_with_deps(
        id: &str,
        title: &str,
        status: Status,
        blocked_by: Vec<&str>,
    ) -> workgraph::graph::Task {
        workgraph::graph::Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            blocked_by: blocked_by.into_iter().map(String::from).collect(),
            ..workgraph::graph::Task::default()
        }
    }

    fn setup_graph(dir: &Path, tasks: Vec<workgraph::graph::Task>) {
        std::fs::create_dir_all(dir).unwrap();
        let graph_file = dir.join("graph.jsonl");
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &graph_file).unwrap();
    }

    fn load_task_ids(dir: &Path) -> HashSet<String> {
        let graph_file = dir.join("graph.jsonl");
        let graph = load_graph(&graph_file).unwrap();
        graph.tasks().map(|t| t.id.clone()).collect()
    }

    #[test]
    fn gc_removes_abandoned_task_no_open_dependents() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task("task-a", "Abandoned task", Status::Abandoned),
                make_task("task-b", "Open task", Status::Open),
            ],
        );

        run(wg_dir, false, false).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            !remaining.contains("task-a"),
            "abandoned task should be removed"
        );
        assert!(remaining.contains("task-b"), "open task should remain");
    }

    #[test]
    fn gc_removes_failed_task_no_open_dependents() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task("task-a", "Failed task", Status::Failed),
                make_task("task-b", "Open task", Status::Open),
            ],
        );

        run(wg_dir, false, false).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            !remaining.contains("task-a"),
            "failed task should be removed"
        );
        assert!(remaining.contains("task-b"), "open task should remain");
    }

    #[test]
    fn gc_does_not_remove_task_blocking_open_task() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task("task-a", "Abandoned blocker", Status::Abandoned),
                make_task_with_deps("task-b", "Open dependent", Status::Open, vec!["task-a"]),
            ],
        );

        run(wg_dir, false, false).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            remaining.contains("task-a"),
            "abandoned task blocking open task should NOT be removed"
        );
        assert!(remaining.contains("task-b"), "open task should remain");
    }

    #[test]
    fn gc_dry_run_shows_but_does_not_remove() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task("task-a", "Abandoned task", Status::Abandoned),
                make_task("task-b", "Open task", Status::Open),
            ],
        );

        run(wg_dir, true, false).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            remaining.contains("task-a"),
            "dry run should not remove anything"
        );
        assert!(remaining.contains("task-b"));
    }

    #[test]
    fn gc_removes_associated_internal_tasks() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task("my-task", "Abandoned task", Status::Abandoned),
                make_task("assign-my-task", "Assign task", Status::Done),
                make_task("evaluate-my-task", "Evaluate task", Status::Done),
                make_task("task-b", "Open task", Status::Open),
            ],
        );

        run(wg_dir, false, false).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(!remaining.contains("my-task"), "parent should be removed");
        assert!(
            !remaining.contains("assign-my-task"),
            "assign- internal task should be removed"
        );
        assert!(
            !remaining.contains("evaluate-my-task"),
            "evaluate- internal task should be removed"
        );
        assert!(remaining.contains("task-b"), "open task should remain");
    }

    #[test]
    fn gc_does_not_remove_done_by_default() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task("task-a", "Done task", Status::Done),
                make_task("task-b", "Abandoned task", Status::Abandoned),
            ],
        );

        run(wg_dir, false, false).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            remaining.contains("task-a"),
            "done task should NOT be removed by default"
        );
        assert!(
            !remaining.contains("task-b"),
            "abandoned task should be removed"
        );
    }

    #[test]
    fn gc_removes_done_with_include_done_flag() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task("task-a", "Done task", Status::Done),
                make_task("task-b", "Abandoned task", Status::Abandoned),
            ],
        );

        run(wg_dir, false, true).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            !remaining.contains("task-a"),
            "done task should be removed with --include-done"
        );
        assert!(
            !remaining.contains("task-b"),
            "abandoned task should be removed"
        );
    }

    #[test]
    fn gc_removes_orphaned_internal_tasks() {
        // Internal tasks whose parent was already archived/removed
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task("assign-old-task", "Stale assign", Status::Done),
                make_task("evaluate-old-task", "Stale evaluate", Status::Abandoned),
                make_task("task-b", "Open task", Status::Open),
            ],
        );

        run(wg_dir, false, false).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            !remaining.contains("assign-old-task"),
            "orphaned assign- task should be removed"
        );
        assert!(
            !remaining.contains("evaluate-old-task"),
            "orphaned evaluate- task should be removed"
        );
        assert!(remaining.contains("task-b"));
    }

    #[test]
    fn gc_does_not_remove_task_blocking_in_progress() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task("task-a", "Failed blocker", Status::Failed),
                make_task_with_deps(
                    "task-b",
                    "In-progress dependent",
                    Status::InProgress,
                    vec!["task-a"],
                ),
            ],
        );

        run(wg_dir, false, false).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            remaining.contains("task-a"),
            "failed task blocking in-progress task should NOT be removed"
        );
    }

    #[test]
    fn gc_empty_graph() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(wg_dir, vec![]);

        run(wg_dir, false, false).unwrap();
        // Should not panic, just print "No tasks to garbage collect."
    }
}
