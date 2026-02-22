use anyhow::Result;
use chrono::{DateTime, Utc};
use std::path::Path;
use workgraph::graph::Status;

pub fn run(dir: &Path, status_filter: Option<&str>, paused_only: bool, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    let status_filter: Option<Status> = match status_filter {
        Some("open") => Some(Status::Open),
        Some("done") => Some(Status::Done),
        Some("in-progress") => Some(Status::InProgress),
        Some("blocked") => Some(Status::Blocked),
        Some("failed") => Some(Status::Failed),
        Some("abandoned") => Some(Status::Abandoned),
        Some(s) => anyhow::bail!(
            "Unknown status: '{}'. Valid values: open, in-progress, done, blocked, failed, abandoned",
            s
        ),
        None => None,
    };

    let tasks: Vec<_> = graph
        .tasks()
        .filter(|t| status_filter.as_ref().is_none_or(|s| &t.status == s))
        .filter(|t| !paused_only || t.paused)
        .collect();

    if json {
        let output: Vec<_> = tasks
            .iter()
            .map(|t| {
                let mut obj = serde_json::json!({
                    "id": t.id,
                    "title": t.title,
                    "status": t.status,
                    "assigned": t.assigned,
                    "after": t.after,
                });
                if let Some(ref ra) = t.ready_after {
                    obj["ready_after"] = serde_json::json!(ra);
                }
                if t.paused {
                    obj["paused"] = serde_json::json!(true);
                }
                obj
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if tasks.is_empty() {
        println!("No tasks found");
    } else {
        for task in tasks {
            let status = match task.status {
                Status::Open => "[ ]",
                Status::InProgress => "[~]",
                Status::Done => "[x]",
                Status::Blocked => "[!]",
                Status::Failed => "[F]",
                Status::Abandoned => "[A]",
            };
            let pause_str = if task.paused { " [PAUSED]" } else { "" };
            let delay_str = format_ready_after_hint(task.ready_after.as_deref());
            println!(
                "{} {} - {}{}{}",
                status, task.id, task.title, pause_str, delay_str
            );
        }
    }

    Ok(())
}

/// If ready_after is set and in the future, return a hint string like " [ready in 5m 30s]".
fn format_ready_after_hint(ready_after: Option<&str>) -> String {
    let Some(ra) = ready_after else {
        return String::new();
    };
    let Ok(ts) = ra.parse::<DateTime<Utc>>() else {
        return String::new();
    };
    let now = Utc::now();
    if ts <= now {
        return String::new(); // Already elapsed
    }
    let secs = (ts - now).num_seconds();
    let countdown = if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h", secs / 86400, (secs % 86400) / 3600)
    };
    format!(" [ready in {}]", countdown)
}

#[cfg(test)]
mod tests {
    use super::super::graph_path;
    use super::*;
    use chrono::Duration;
    use std::fs;
    use tempfile::tempdir;
    use workgraph::graph::{Node, Task, WorkGraph};
    use workgraph::parser::{load_graph, save_graph};

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

    // --- format_ready_after_hint tests ---

    #[test]
    fn test_hint_none_ready_after() {
        assert_eq!(format_ready_after_hint(None), "");
    }

    #[test]
    fn test_hint_past_timestamp_returns_empty() {
        let past = (Utc::now() - Duration::hours(1)).to_rfc3339();
        assert_eq!(format_ready_after_hint(Some(&past)), "");
    }

    #[test]
    fn test_hint_invalid_timestamp_returns_empty() {
        assert_eq!(format_ready_after_hint(Some("not-a-timestamp")), "");
    }

    #[test]
    fn test_hint_future_seconds() {
        let future = (Utc::now() + Duration::seconds(30)).to_rfc3339();
        let result = format_ready_after_hint(Some(&future));
        assert!(result.starts_with(" [ready in "));
        assert!(result.ends_with("s]"));
        assert!(!result.contains('m'));
    }

    #[test]
    fn test_hint_future_minutes() {
        let future = (Utc::now() + Duration::minutes(5) + Duration::seconds(10)).to_rfc3339();
        let result = format_ready_after_hint(Some(&future));
        assert!(result.starts_with(" [ready in 5m"));
        assert!(result.ends_with("s]"));
    }

    #[test]
    fn test_hint_future_hours() {
        let future = (Utc::now() + Duration::hours(2) + Duration::minutes(15)).to_rfc3339();
        let result = format_ready_after_hint(Some(&future));
        assert!(result.starts_with(" [ready in 2h"));
        assert!(result.ends_with("m]"));
    }

    #[test]
    fn test_hint_future_days() {
        let future = (Utc::now() + Duration::days(3) + Duration::hours(6)).to_rfc3339();
        let result = format_ready_after_hint(Some(&future));
        assert!(result.starts_with(" [ready in 3d"));
        assert!(result.ends_with("h]"));
    }

    // --- run() tests: status filtering ---

    #[test]
    fn test_run_uninitialized() {
        let dir = tempdir().unwrap();
        let result = run(dir.path(), None, false, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not initialized"));
    }

    #[test]
    fn test_run_no_tasks() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![]);
        let result = run(dir.path(), None, false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_status_filter_open() {
        let dir = tempdir().unwrap();
        setup_workgraph(
            dir.path(),
            vec![
                make_task("t1", "Open task", Status::Open),
                make_task("t2", "Done task", Status::Done),
                make_task("t3", "In-progress task", Status::InProgress),
            ],
        );
        let result = run(dir.path(), Some("open"), false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_status_filter_done() {
        let dir = tempdir().unwrap();
        setup_workgraph(
            dir.path(),
            vec![
                make_task("t1", "Open task", Status::Open),
                make_task("t2", "Done task", Status::Done),
            ],
        );
        let result = run(dir.path(), Some("done"), false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_status_filter_in_progress() {
        let dir = tempdir().unwrap();
        setup_workgraph(
            dir.path(),
            vec![make_task("t1", "IP task", Status::InProgress)],
        );
        let result = run(dir.path(), Some("in-progress"), false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_status_filter_blocked() {
        let dir = tempdir().unwrap();
        setup_workgraph(
            dir.path(),
            vec![make_task("t1", "Blocked task", Status::Blocked)],
        );
        let result = run(dir.path(), Some("blocked"), false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_unknown_status_filter() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![make_task("t1", "Task", Status::Open)]);
        let result = run(dir.path(), Some("nonexistent-status"), false, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown status"));
    }

    #[test]
    fn test_status_filter_logic() {
        let dir = tempdir().unwrap();
        let tasks = vec![
            make_task("t-open", "Open", Status::Open),
            make_task("t-done", "Done", Status::Done),
            make_task("t-ip", "InProgress", Status::InProgress),
            make_task("t-blocked", "Blocked", Status::Blocked),
        ];
        let path = setup_workgraph(dir.path(), tasks);
        let graph = load_graph(&path).unwrap();

        let open: Vec<_> = graph.tasks().filter(|t| t.status == Status::Open).collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, "t-open");

        let done: Vec<_> = graph.tasks().filter(|t| t.status == Status::Done).collect();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].id, "t-done");

        let ip: Vec<_> = graph
            .tasks()
            .filter(|t| t.status == Status::InProgress)
            .collect();
        assert_eq!(ip.len(), 1);
        assert_eq!(ip[0].id, "t-ip");

        let blocked: Vec<_> = graph
            .tasks()
            .filter(|t| t.status == Status::Blocked)
            .collect();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].id, "t-blocked");

        let all: Vec<_> = graph.tasks().collect();
        assert_eq!(all.len(), 4);
    }

    // --- run() tests: ready_after display ---

    #[test]
    fn test_run_task_with_ready_after_display() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Delayed task", Status::Open);
        let future = Utc::now() + Duration::hours(1);
        task.ready_after = Some(future.to_rfc3339());
        setup_workgraph(dir.path(), vec![task]);

        let result = run(dir.path(), None, false, false);
        assert!(result.is_ok());
    }

    // --- run() tests: JSON output ---

    #[test]
    fn test_run_json_output() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "JSON task", Status::Open);
        task.assigned = Some("agent-1".to_string());
        task.after = vec!["dep-1".to_string()];
        setup_workgraph(dir.path(), vec![task]);

        let result = run(dir.path(), None, false, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_json_output_structure() {
        let dir = tempdir().unwrap();
        let mut task = make_task("t1", "Structured", Status::Open);
        task.assigned = Some("agent-x".to_string());
        task.after = vec!["dep-a".to_string()];
        let future = Utc::now() + Duration::hours(1);
        let future_str = future.to_rfc3339();
        task.ready_after = Some(future_str.clone());
        let path = setup_workgraph(dir.path(), vec![task]);

        let graph = load_graph(&path).unwrap();
        let tasks: Vec<_> = graph.tasks().collect();
        assert_eq!(tasks.len(), 1);
        let t = tasks[0];

        let mut obj = serde_json::json!({
            "id": t.id,
            "title": t.title,
            "status": t.status,
            "assigned": t.assigned,
            "after": t.after,
        });
        if let Some(ref ra) = t.ready_after {
            obj["ready_after"] = serde_json::json!(ra);
        }

        assert_eq!(obj["id"], "t1");
        assert_eq!(obj["title"], "Structured");
        assert_eq!(obj["status"], "open");
        assert_eq!(obj["assigned"], "agent-x");
        assert_eq!(obj["after"][0], "dep-a");
        assert_eq!(obj["ready_after"], future_str);
    }

    #[test]
    fn test_json_output_no_ready_after_when_absent() {
        let dir = tempdir().unwrap();
        let task = make_task("t1", "No delay", Status::Open);
        let path = setup_workgraph(dir.path(), vec![task]);

        let graph = load_graph(&path).unwrap();
        let t = graph.get_task("t1").unwrap();

        let mut obj = serde_json::json!({
            "id": t.id,
            "title": t.title,
            "status": t.status,
            "assigned": t.assigned,
            "after": t.after,
        });
        if let Some(ref ra) = t.ready_after {
            obj["ready_after"] = serde_json::json!(ra);
        }

        assert!(obj.get("ready_after").is_none());
    }

    #[test]
    fn test_run_status_filter_failed() {
        let dir = tempdir().unwrap();
        setup_workgraph(
            dir.path(),
            vec![
                make_task("t1", "Failed task", Status::Failed),
                make_task("t2", "Open task", Status::Open),
            ],
        );
        let result = run(dir.path(), Some("failed"), false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_status_filter_abandoned() {
        let dir = tempdir().unwrap();
        setup_workgraph(
            dir.path(),
            vec![
                make_task("t1", "Abandoned task", Status::Abandoned),
                make_task("t2", "Open task", Status::Open),
            ],
        );
        let result = run(dir.path(), Some("abandoned"), false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_unknown_status_error_lists_valid_values() {
        let dir = tempdir().unwrap();
        setup_workgraph(dir.path(), vec![make_task("t1", "Task", Status::Open)]);
        let result = run(dir.path(), Some("bogus"), false, false);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Valid values:"));
        assert!(msg.contains("failed"));
        assert!(msg.contains("abandoned"));
    }

    #[test]
    fn test_status_filter_logic_failed_and_abandoned() {
        let dir = tempdir().unwrap();
        let tasks = vec![
            make_task("t-open", "Open", Status::Open),
            make_task("t-failed", "Failed", Status::Failed),
            make_task("t-abandoned", "Abandoned", Status::Abandoned),
        ];
        let path = setup_workgraph(dir.path(), tasks);
        let graph = load_graph(&path).unwrap();

        let failed: Vec<_> = graph
            .tasks()
            .filter(|t| t.status == Status::Failed)
            .collect();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].id, "t-failed");

        let abandoned: Vec<_> = graph
            .tasks()
            .filter(|t| t.status == Status::Abandoned)
            .collect();
        assert_eq!(abandoned.len(), 1);
        assert_eq!(abandoned[0].id, "t-abandoned");
    }

    #[test]
    fn test_run_json_with_status_filter() {
        let dir = tempdir().unwrap();
        setup_workgraph(
            dir.path(),
            vec![
                make_task("t1", "Open", Status::Open),
                make_task("t2", "Done", Status::Done),
            ],
        );
        let result = run(dir.path(), Some("done"), false, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_paused_filter() {
        let dir = tempdir().unwrap();
        let mut paused_task = make_task("t-paused", "Paused task", Status::Open);
        paused_task.paused = true;
        let mut paused_ip = make_task("t-paused-ip", "Paused IP task", Status::InProgress);
        paused_ip.paused = true;
        let normal_task = make_task("t-normal", "Normal task", Status::Open);

        let path = setup_workgraph(dir.path(), vec![paused_task, paused_ip, normal_task]);
        let graph = load_graph(&path).unwrap();

        // --paused flag filters to only paused tasks
        let paused: Vec<_> = graph.tasks().filter(|t| t.paused).collect();
        assert_eq!(paused.len(), 2);
        assert!(paused.iter().all(|t| t.paused));

        // --paused combined with --status open
        let paused_open: Vec<_> = graph
            .tasks()
            .filter(|t| t.status == Status::Open)
            .filter(|t| t.paused)
            .collect();
        assert_eq!(paused_open.len(), 1);
        assert_eq!(paused_open[0].id, "t-paused");

        // run() with paused_only=true should succeed
        let result = run(dir.path(), None, true, false);
        assert!(result.is_ok());

        // run() with paused_only=true and status filter should succeed
        let result = run(dir.path(), Some("open"), true, false);
        assert!(result.is_ok());
    }
}
