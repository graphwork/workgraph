use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::collections::HashSet;
use std::path::Path;
use workgraph::graph::Status;
use workgraph::parser::modify_graph;

use super::graph_path;

/// Auto-generated task prefixes that should be gc'd alongside their parent task.
const INTERNAL_PREFIXES: &[&str] = &[
    ".assign-",
    ".evaluate-",
    ".verify-",
    ".respond-to-",
    "assign-",
    "evaluate-",
];

/// Get the best available terminal timestamp for a task.
/// For done tasks, uses completed_at. For failed/abandoned, uses the last log
/// entry timestamp (which is when fail/abandon was called), then falls back to
/// started_at, then created_at.
fn terminal_timestamp(task: &workgraph::graph::Task) -> Option<DateTime<chrono::FixedOffset>> {
    // Done tasks have completed_at set by the done command
    if let Some(ref s) = task.completed_at
        && let Ok(ts) = DateTime::parse_from_rfc3339(s)
    {
        return Some(ts);
    }
    // Failed/Abandoned: the fail/abandon command adds a log entry with timestamp
    if let Some(entry) = task.log.last()
        && let Ok(ts) = DateTime::parse_from_rfc3339(&entry.timestamp)
    {
        return Some(ts);
    }
    // Fallback chain
    let ts = task.started_at.as_deref().or(task.created_at.as_deref())?;
    DateTime::parse_from_rfc3339(ts).ok()
}

/// Check if a task is old enough to gc based on the --older filter.
fn is_old_enough(task: &workgraph::graph::Task, min_age: &chrono::Duration) -> bool {
    if let Some(ts) = terminal_timestamp(task) {
        let age = Utc::now().signed_duration_since(ts);
        age > *min_age
    } else {
        // No timestamp available — skip when --older is specified
        false
    }
}

pub fn run(dir: &Path, dry_run: bool, include_done: bool, older: Option<&str>) -> Result<()> {
    let path = graph_path(dir);
    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    // Parse --older duration if provided
    let older_duration = if let Some(older_str) = older {
        Some(super::archive::parse_duration(older_str)?)
    } else {
        None
    };

    let mut gc_list: Vec<String> = Vec::new();
    let mut removed_details: Vec<serde_json::Value> = Vec::new();
    let mut display_lines: Vec<String> = Vec::new();
    let mut was_empty = false;

    modify_graph(&path, |graph| {
        // Collect all task IDs and their statuses for dependency checking
        let all_tasks: Vec<_> = graph.tasks().cloned().collect();

        // Build a set of task IDs that have non-terminal dependents.
        let mut has_open_dependent: HashSet<String> = HashSet::new();
        for task in &all_tasks {
            if !task.status.is_terminal() {
                for blocker_id in &task.after {
                    has_open_dependent.insert(blocker_id.clone());
                }
            }
        }

        // SCC-aware gc
        let cycle_analysis = graph.compute_cycle_analysis();

        let mut protected_scc_members: HashSet<String> = HashSet::new();
        let mut scc_gc_candidates: Vec<Vec<String>> = Vec::new();

        for cycle in &cycle_analysis.cycles {
            let all_terminal = cycle
                .members
                .iter()
                .all(|id| graph.get_task(id).is_some_and(|t| t.status.is_terminal()));

            let has_external_dependent = cycle.members.iter().any(|id| {
                all_tasks.iter().any(|t| {
                    !t.status.is_terminal() && t.after.contains(id) && !cycle.members.contains(&t.id)
                })
            });

            if !all_terminal || has_external_dependent {
                for id in &cycle.members {
                    protected_scc_members.insert(id.clone());
                }
            } else {
                scc_gc_candidates.push(cycle.members.clone());
            }
        }

        let mut to_gc: HashSet<String> = HashSet::new();
        for task in &all_tasks {
            if !task.status.is_terminal() {
                continue;
            }
            if task.status == Status::Done && !include_done {
                continue;
            }
            if protected_scc_members.contains(&task.id) {
                continue;
            }
            if has_open_dependent.contains(&task.id) {
                continue;
            }
            if let Some(ref min_age) = older_duration
                && !is_old_enough(task, min_age)
            {
                continue;
            }
            to_gc.insert(task.id.clone());
        }

        for members in &scc_gc_candidates {
            let all_pass_status = members.iter().all(|id| {
                graph
                    .get_task(id)
                    .is_some_and(|t| t.status != Status::Done || include_done)
            });
            if !all_pass_status {
                continue;
            }

            if let Some(ref min_age) = older_duration {
                let all_old_enough = members.iter().all(|id| {
                    graph
                        .get_task(id)
                        .is_some_and(|t| is_old_enough(t, min_age))
                });
                if !all_old_enough {
                    continue;
                }
            }

            for id in members {
                to_gc.insert(id.clone());
            }
        }

        // Also collect internal tasks whose parent is being gc'd
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

        for task in &all_tasks {
            if to_gc.contains(&task.id) {
                continue;
            }
            let is_internal = INTERNAL_PREFIXES
                .iter()
                .any(|prefix| task.id.starts_with(prefix));
            if is_internal && task.status.is_terminal() && !has_open_dependent.contains(&task.id) {
                if let Some(ref min_age) = older_duration
                    && !is_old_enough(task, min_age)
                {
                    continue;
                }
                to_gc.insert(task.id.clone());
            }
        }

        if to_gc.is_empty() {
            was_empty = true;
            return false;
        }

        gc_list = to_gc.iter().cloned().collect();
        gc_list.sort();

        if dry_run {
            for id in &gc_list {
                if let Some(task) = graph.get_task(id) {
                    display_lines.push(format!("  {} - {} [{}]", task.id, task.title, task.status));
                }
            }
            return false;
        }

        // Capture provenance data
        removed_details = gc_list
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

        for id in &gc_list {
            graph.remove_node(id);
        }
        true
    })
    .context("Failed to modify graph")?;

    if was_empty {
        println!("No tasks to garbage collect.");
        return Ok(());
    }

    if dry_run {
        println!("Would remove {} tasks:", gc_list.len());
        for line in &display_lines {
            println!("{}", line);
        }
        return Ok(());
    }

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
    use chrono::Duration;
    use tempfile::tempdir;
    use workgraph::graph::{LogEntry, Node, WorkGraph};

    fn make_task(id: &str, title: &str, status: Status) -> workgraph::graph::Task {
        workgraph::graph::Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            ..workgraph::graph::Task::default()
        }
    }

    fn make_task_with_timestamp(
        id: &str,
        title: &str,
        status: Status,
        completed_at: Option<&str>,
    ) -> workgraph::graph::Task {
        workgraph::graph::Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            completed_at: completed_at.map(String::from),
            ..workgraph::graph::Task::default()
        }
    }

    fn make_task_with_deps(
        id: &str,
        title: &str,
        status: Status,
        after: Vec<&str>,
    ) -> workgraph::graph::Task {
        workgraph::graph::Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            after: after.into_iter().map(String::from).collect(),
            ..workgraph::graph::Task::default()
        }
    }

    fn make_task_with_deps_and_timestamp(
        id: &str,
        title: &str,
        status: Status,
        after: Vec<&str>,
        completed_at: Option<&str>,
    ) -> workgraph::graph::Task {
        workgraph::graph::Task {
            id: id.to_string(),
            title: title.to_string(),
            status,
            after: after.into_iter().map(String::from).collect(),
            completed_at: completed_at.map(String::from),
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

        run(wg_dir, false, false, None).unwrap();

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

        run(wg_dir, false, false, None).unwrap();

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

        run(wg_dir, false, false, None).unwrap();

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

        run(wg_dir, true, false, None).unwrap();

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

        run(wg_dir, false, false, None).unwrap();

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

        run(wg_dir, false, false, None).unwrap();

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

        run(wg_dir, false, true, None).unwrap();

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

        run(wg_dir, false, false, None).unwrap();

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

        run(wg_dir, false, false, None).unwrap();

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

        run(wg_dir, false, false, None).unwrap();
        // Should not panic, just print "No tasks to garbage collect."
    }

    // --older flag tests

    #[test]
    fn gc_older_skips_recent_failed_tasks() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let recent = (Utc::now() - Duration::hours(2)).to_rfc3339();
        setup_graph(
            wg_dir,
            vec![
                make_task_with_timestamp("task-a", "Recent failed", Status::Failed, Some(&recent)),
                make_task("task-b", "Open task", Status::Open),
            ],
        );

        run(wg_dir, false, false, Some("7d")).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            remaining.contains("task-a"),
            "recent failed task should NOT be removed with --older 7d"
        );
    }

    #[test]
    fn gc_older_removes_old_failed_tasks() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let old = (Utc::now() - Duration::days(10)).to_rfc3339();
        setup_graph(
            wg_dir,
            vec![
                make_task_with_timestamp("task-a", "Old failed", Status::Failed, Some(&old)),
                make_task("task-b", "Open task", Status::Open),
            ],
        );

        run(wg_dir, false, false, Some("7d")).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            !remaining.contains("task-a"),
            "old failed task should be removed with --older 7d"
        );
    }

    #[test]
    fn gc_older_with_include_done_removes_old_done_tasks() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let old = (Utc::now() - Duration::days(40)).to_rfc3339();
        let recent = (Utc::now() - Duration::days(5)).to_rfc3339();
        setup_graph(
            wg_dir,
            vec![
                make_task_with_timestamp("task-old", "Old done", Status::Done, Some(&old)),
                make_task_with_timestamp("task-recent", "Recent done", Status::Done, Some(&recent)),
            ],
        );

        run(wg_dir, false, true, Some("30d")).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            !remaining.contains("task-old"),
            "old done task should be removed with --include-done --older 30d"
        );
        assert!(
            remaining.contains("task-recent"),
            "recent done task should NOT be removed with --older 30d"
        );
    }

    #[test]
    fn gc_older_skips_tasks_without_timestamps() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task("task-a", "Failed no timestamp", Status::Failed),
                make_task("task-b", "Open task", Status::Open),
            ],
        );

        run(wg_dir, false, false, Some("7d")).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            remaining.contains("task-a"),
            "failed task without timestamp should NOT be removed with --older"
        );
    }

    #[test]
    fn gc_older_uses_log_timestamp_for_failed() {
        // Failed tasks don't have completed_at — terminal_timestamp should
        // fall back to the last log entry (set by fail command)
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let old = (Utc::now() - Duration::days(10)).to_rfc3339();
        let mut task = make_task("task-a", "Old failed via log", Status::Failed);
        task.log.push(LogEntry {
            timestamp: old,
            actor: None,
            user: Some(workgraph::current_user()),
            message: "Task marked as failed".to_string(),
        });
        setup_graph(wg_dir, vec![task]);

        run(wg_dir, false, false, Some("7d")).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            !remaining.contains("task-a"),
            "failed task with old log entry should be gc'd with --older 7d"
        );
    }

    #[test]
    fn gc_older_log_timestamp_skips_recent_failed() {
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let recent = (Utc::now() - Duration::hours(2)).to_rfc3339();
        let mut task = make_task("task-a", "Recent failed via log", Status::Failed);
        task.log.push(LogEntry {
            timestamp: recent,
            actor: None,
            user: Some(workgraph::current_user()),
            message: "Task marked as failed".to_string(),
        });
        setup_graph(wg_dir, vec![task]);

        run(wg_dir, false, false, Some("7d")).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            remaining.contains("task-a"),
            "failed task with recent log entry should NOT be gc'd with --older 7d"
        );
    }

    // SCC-aware gc tests

    #[test]
    fn gc_removes_completed_scc_as_unit() {
        // Two tasks in a cycle, both done — should be gc'd as a unit
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let old = (Utc::now() - Duration::days(10)).to_rfc3339();
        setup_graph(
            wg_dir,
            vec![
                make_task_with_deps_and_timestamp(
                    "cycle-a",
                    "Cycle A",
                    Status::Done,
                    vec!["cycle-b"],
                    Some(&old),
                ),
                make_task_with_deps_and_timestamp(
                    "cycle-b",
                    "Cycle B",
                    Status::Done,
                    vec!["cycle-a"],
                    Some(&old),
                ),
                make_task("unrelated", "Open task", Status::Open),
            ],
        );

        // With --include-done since both are done
        run(wg_dir, false, true, None).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            !remaining.contains("cycle-a"),
            "SCC member cycle-a should be removed"
        );
        assert!(
            !remaining.contains("cycle-b"),
            "SCC member cycle-b should be removed"
        );
        assert!(
            remaining.contains("unrelated"),
            "unrelated task should remain"
        );
    }

    #[test]
    fn gc_does_not_remove_scc_with_non_terminal_member() {
        // Two tasks in a cycle, one still open — should NOT be gc'd
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task_with_deps("cycle-a", "Cycle A", Status::Done, vec!["cycle-b"]),
                make_task_with_deps("cycle-b", "Cycle B", Status::Open, vec!["cycle-a"]),
            ],
        );

        run(wg_dir, false, true, None).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            remaining.contains("cycle-a"),
            "SCC with open member should NOT be gc'd"
        );
        assert!(remaining.contains("cycle-b"));
    }

    #[test]
    fn gc_does_not_remove_scc_with_external_dependent() {
        // Completed SCC but an external open task depends on one member
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        setup_graph(
            wg_dir,
            vec![
                make_task_with_deps("cycle-a", "Cycle A", Status::Done, vec!["cycle-b"]),
                make_task_with_deps("cycle-b", "Cycle B", Status::Done, vec!["cycle-a"]),
                make_task_with_deps(
                    "external",
                    "Depends on cycle",
                    Status::Open,
                    vec!["cycle-a"],
                ),
            ],
        );

        run(wg_dir, false, true, None).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            remaining.contains("cycle-a"),
            "SCC with external dependent should NOT be gc'd"
        );
        assert!(remaining.contains("cycle-b"));
    }

    #[test]
    fn gc_scc_respects_older_filter() {
        // Completed SCC but members are too recent
        let dir = tempdir().unwrap();
        let wg_dir = dir.path();
        let recent = (Utc::now() - Duration::days(2)).to_rfc3339();
        setup_graph(
            wg_dir,
            vec![
                make_task_with_deps_and_timestamp(
                    "cycle-a",
                    "Cycle A",
                    Status::Done,
                    vec!["cycle-b"],
                    Some(&recent),
                ),
                make_task_with_deps_and_timestamp(
                    "cycle-b",
                    "Cycle B",
                    Status::Done,
                    vec!["cycle-a"],
                    Some(&recent),
                ),
            ],
        );

        run(wg_dir, false, true, Some("7d")).unwrap();

        let remaining = load_task_ids(wg_dir);
        assert!(
            remaining.contains("cycle-a"),
            "recent SCC should NOT be removed with --older 7d"
        );
        assert!(remaining.contains("cycle-b"));
    }
}
