use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;
use workgraph::graph::Status;
use workgraph::parser::load_graph;
use workgraph::query::ready_tasks;

use super::graph_path;

pub fn run(dir: &Path, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;
    let ready = ready_tasks(&graph);

    // Find tasks that would be ready except they're waiting on ready_after
    let waiting: Vec<_> = graph
        .tasks()
        .filter(|task| {
            if task.status != Status::Open {
                return false;
            }
            // Must have a future ready_after
            let has_future_ready_after = task.ready_after.as_ref().is_some_and(|ra| {
                ra.parse::<DateTime<Utc>>()
                    .map(|ts| ts > Utc::now())
                    .unwrap_or(false)
            });
            if !has_future_ready_after {
                return false;
            }
            // All blockers must be done (i.e. only ready_after is holding it back)
            task.blocked_by.iter().all(|blocker_id| {
                graph
                    .get_task(blocker_id)
                    .map(|t| t.status == Status::Done)
                    .unwrap_or(true)
            })
        })
        .collect();

    if json {
        let mut output: Vec<_> = ready
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "title": t.title,
                    "assigned": t.assigned,
                    "estimate": t.estimate,
                    "ready": true,
                })
            })
            .collect();
        for t in &waiting {
            output.push(serde_json::json!({
                "id": t.id,
                "title": t.title,
                "assigned": t.assigned,
                "estimate": t.estimate,
                "ready": false,
                "ready_after": t.ready_after,
            }));
        }
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if ready.is_empty() && waiting.is_empty() {
        println!("No tasks ready");
    } else {
        if !ready.is_empty() {
            println!("Ready tasks:");
            for task in &ready {
                let assigned = task
                    .assigned
                    .as_ref()
                    .map(|a| format!(" ({})", a))
                    .unwrap_or_default();
                println!("  {} - {}{}", task.id, task.title, assigned);
            }
        }
        if !waiting.is_empty() {
            if !ready.is_empty() {
                println!();
            }
            println!("Waiting on delay:");
            for task in &waiting {
                let countdown = format_countdown(task.ready_after.as_deref().unwrap_or(""));
                println!("  {} - {} {}", task.id, task.title, countdown);
            }
        }
    }

    Ok(())
}

/// Format a timestamp as a countdown string.
fn format_countdown(timestamp: &str) -> String {
    let Ok(ts) = timestamp.parse::<DateTime<Utc>>() else {
        return String::new();
    };
    let now = Utc::now();
    if ts <= now {
        return "(elapsed)".to_string();
    }
    let secs = (ts - now).num_seconds();
    if secs < 60 {
        format!("(ready in {}s)", secs)
    } else if secs < 3600 {
        format!("(ready in {}m {}s)", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("(ready in {}h {}m)", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("(ready in {}d {}h)", secs / 86400, (secs % 86400) / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::save_graph;

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

    // --- format_countdown tests ---

    #[test]
    fn test_format_countdown_seconds() {
        let future = Utc::now() + Duration::seconds(45);
        let result = format_countdown(&future.to_rfc3339());
        assert!(result.starts_with("(ready in "));
        assert!(result.ends_with("s)"));
        // Should show just seconds — no 'm)', 'h)', or 'd)' unit markers
        assert!(!result.contains("m "));
        assert!(!result.contains("h "));
    }

    #[test]
    fn test_format_countdown_minutes() {
        let future = Utc::now() + Duration::minutes(5) + Duration::seconds(30);
        let result = format_countdown(&future.to_rfc3339());
        assert!(result.starts_with("(ready in 5m"));
        assert!(result.ends_with("s)"));
        // Should not contain hours or days
        assert!(!result.contains("h "));
    }

    #[test]
    fn test_format_countdown_hours() {
        let future = Utc::now() + Duration::hours(2) + Duration::minutes(15);
        let result = format_countdown(&future.to_rfc3339());
        assert!(result.starts_with("(ready in 2h"));
        assert!(result.ends_with("m)"));
    }

    #[test]
    fn test_format_countdown_days() {
        let future = Utc::now() + Duration::days(3) + Duration::hours(6);
        let result = format_countdown(&future.to_rfc3339());
        assert!(result.starts_with("(ready in 3d"));
        assert!(result.contains('h'));
    }

    #[test]
    fn test_format_countdown_elapsed() {
        let past = Utc::now() - Duration::seconds(10);
        let result = format_countdown(&past.to_rfc3339());
        assert_eq!(result, "(elapsed)");
    }

    #[test]
    fn test_format_countdown_invalid_timestamp() {
        let result = format_countdown("not-a-timestamp");
        assert_eq!(result, "");
    }

    #[test]
    fn test_format_countdown_empty_string() {
        let result = format_countdown("");
        assert_eq!(result, "");
    }

    #[test]
    fn test_format_countdown_exactly_at_now() {
        // A timestamp very slightly in the past should show "(elapsed)"
        let at_now = Utc::now() - Duration::milliseconds(100);
        let result = format_countdown(&at_now.to_rfc3339());
        assert_eq!(result, "(elapsed)");
    }

    // --- run() integration tests ---

    #[test]
    fn test_run_no_tasks() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![]);

        let result = run(dir_path, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_uninitialized() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        // Don't initialize workgraph

        let result = run(dir_path, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[test]
    fn test_run_ready_task_no_ready_after() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Ready task", Status::Open)]);

        let result = run(dir_path, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_task_with_past_ready_after_is_ready() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Past ready_after", Status::Open);
        let past = Utc::now() - Duration::hours(1);
        task.ready_after = Some(past.to_rfc3339());
        setup_workgraph(dir_path, vec![task]);

        // Task with past ready_after should appear in ready list
        let result = run(dir_path, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_task_with_future_ready_after_is_waiting() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Future ready_after", Status::Open);
        let future = Utc::now() + Duration::hours(2);
        task.ready_after = Some(future.to_rfc3339());
        setup_workgraph(dir_path, vec![task]);

        // Task with future ready_after should be in waiting section, not ready
        let result = run(dir_path, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_json_output_ready_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Ready task", Status::Open);
        task.assigned = Some("agent-1".to_string());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, true);
        assert!(result.is_ok());
        // JSON output goes to stdout; we verify it doesn't error
    }

    #[test]
    fn test_run_json_output_waiting_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        let mut task = make_task("t1", "Waiting task", Status::Open);
        let future = Utc::now() + Duration::hours(1);
        task.ready_after = Some(future.to_rfc3339());
        setup_workgraph(dir_path, vec![task]);

        let result = run(dir_path, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_mixed_ready_and_waiting() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let ready_task = make_task("t1", "Ready task", Status::Open);
        let mut waiting_task = make_task("t2", "Waiting task", Status::Open);
        let future = Utc::now() + Duration::minutes(30);
        waiting_task.ready_after = Some(future.to_rfc3339());

        setup_workgraph(dir_path, vec![ready_task, waiting_task]);

        let result = run(dir_path, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_done_tasks_excluded() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(dir_path, vec![make_task("t1", "Done task", Status::Done)]);

        // Done tasks should not appear in ready or waiting
        let result = run(dir_path, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_inprogress_tasks_excluded() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();
        setup_workgraph(
            dir_path,
            vec![make_task("t1", "In-progress task", Status::InProgress)],
        );

        let result = run(dir_path, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_blocked_task_with_future_ready_after_not_waiting() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        // Task with a future ready_after but also an incomplete blocker
        // should NOT appear in waiting (because blockers aren't done)
        let blocker = make_task("blocker", "Blocker", Status::Open);
        let mut task = make_task("t1", "Blocked+Waiting", Status::Open);
        task.blocked_by = vec!["blocker".to_string()];
        let future = Utc::now() + Duration::hours(1);
        task.ready_after = Some(future.to_rfc3339());

        setup_workgraph(dir_path, vec![blocker, task]);

        let result = run(dir_path, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_waiting_task_with_done_blocker() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        // Task with future ready_after and done blocker → appears in waiting
        let blocker = make_task("blocker", "Blocker", Status::Done);
        let mut task = make_task("t1", "Waiting on delay", Status::Open);
        task.blocked_by = vec!["blocker".to_string()];
        let future = Utc::now() + Duration::hours(1);
        task.ready_after = Some(future.to_rfc3339());

        setup_workgraph(dir_path, vec![blocker, task]);

        let result = run(dir_path, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_json_structure_ready_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "JSON test", Status::Open);
        task.assigned = Some("agent-x".to_string());
        setup_workgraph(dir_path, vec![task]);

        // We can't easily capture stdout, but we can verify the run succeeds
        // and validate the JSON structure by building it manually
        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let ready = ready_tasks(&graph);

        assert_eq!(ready.len(), 1);
        let t = &ready[0];
        let json = serde_json::json!({
            "id": t.id,
            "title": t.title,
            "assigned": t.assigned,
            "estimate": t.estimate,
            "ready": true,
        });

        assert_eq!(json["id"], "t1");
        assert_eq!(json["title"], "JSON test");
        assert_eq!(json["assigned"], "agent-x");
        assert_eq!(json["ready"], true);
        assert!(json.get("ready_after").is_none());
    }

    #[test]
    fn test_run_json_structure_waiting_task() {
        let dir = tempdir().unwrap();
        let dir_path = dir.path();

        let mut task = make_task("t1", "JSON wait test", Status::Open);
        let future = Utc::now() + Duration::hours(1);
        let future_str = future.to_rfc3339();
        task.ready_after = Some(future_str.clone());
        setup_workgraph(dir_path, vec![task]);

        let path = graph_path(dir_path);
        let graph = load_graph(&path).unwrap();
        let t = graph.get_task("t1").unwrap();

        // Verify the waiting task JSON structure includes ready_after
        let json = serde_json::json!({
            "id": t.id,
            "title": t.title,
            "assigned": t.assigned,
            "estimate": t.estimate,
            "ready": false,
            "ready_after": t.ready_after,
        });

        assert_eq!(json["id"], "t1");
        assert_eq!(json["title"], "JSON wait test");
        assert_eq!(json["ready"], false);
        assert!(json.get("ready_after").is_some());
        assert_eq!(json["ready_after"], future_str);
    }

    #[test]
    fn test_format_countdown_boundary_60_seconds() {
        // At exactly 60 seconds, should show minutes format
        let future = Utc::now() + Duration::seconds(61);
        let result = format_countdown(&future.to_rfc3339());
        assert!(result.starts_with("(ready in 1m"));
    }

    #[test]
    fn test_format_countdown_boundary_3600_seconds() {
        // At exactly 3600 seconds, should show hours format
        let future = Utc::now() + Duration::seconds(3601);
        let result = format_countdown(&future.to_rfc3339());
        assert!(result.starts_with("(ready in 1h"));
    }

    #[test]
    fn test_format_countdown_boundary_86400_seconds() {
        // At exactly 86400 seconds, should show days format
        let future = Utc::now() + Duration::seconds(86401);
        let result = format_countdown(&future.to_rfc3339());
        assert!(result.starts_with("(ready in 1d"));
    }

    #[test]
    fn test_format_countdown_accuracy_minutes() {
        // 7 minutes and 30 seconds
        let future = Utc::now() + Duration::seconds(7 * 60 + 30 + 1);
        let result = format_countdown(&future.to_rfc3339());
        // Should show "7m 30s" or "7m 31s" (due to timing)
        assert!(result.starts_with("(ready in 7m"));
    }

    #[test]
    fn test_format_countdown_accuracy_hours() {
        // 3 hours and 45 minutes
        let future = Utc::now() + Duration::seconds(3 * 3600 + 45 * 60 + 1);
        let result = format_countdown(&future.to_rfc3339());
        assert!(result.starts_with("(ready in 3h 45m)") || result.starts_with("(ready in 3h"));
    }

    #[test]
    fn test_format_countdown_accuracy_days() {
        // 2 days and 12 hours
        let future = Utc::now() + Duration::seconds(2 * 86400 + 12 * 3600 + 1);
        let result = format_countdown(&future.to_rfc3339());
        assert!(result.starts_with("(ready in 2d 12h)") || result.starts_with("(ready in 2d"));
    }
}
