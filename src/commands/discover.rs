//! `wg discover` — Show recently completed tasks with their artifacts.
//!
//! Designed for agents to call at session start to see what other agents
//! have recently accomplished, enabling stigmergic coordination.

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeMap;
use std::path::Path;

use workgraph::graph::Status;

/// Parse a duration string like "24h", "7d", "30m" into a chrono Duration.
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("Empty duration string");
    }

    let (num_str, unit) = if let Some(stripped) = s.strip_suffix('h') {
        (stripped, 'h')
    } else if let Some(stripped) = s.strip_suffix('d') {
        (stripped, 'd')
    } else if let Some(stripped) = s.strip_suffix('m') {
        (stripped, 'm')
    } else {
        // Default to hours if no unit
        (s, 'h')
    };

    let num: i64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("Invalid duration number: '{}'", num_str))?;

    match unit {
        'm' => Ok(Duration::minutes(num)),
        'h' => Ok(Duration::hours(num)),
        'd' => Ok(Duration::days(num)),
        _ => unreachable!(),
    }
}

pub fn run(dir: &Path, since: Option<&str>, with_artifacts: bool, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    let duration = match since {
        Some(s) => parse_duration(s)?,
        None => Duration::hours(24),
    };
    let cutoff = Utc::now() - duration;

    // Collect recently completed tasks
    let mut recent_tasks: Vec<_> = graph
        .tasks()
        .filter(|task| {
            task.status == Status::Done
                && task
                    .completed_at
                    .as_ref()
                    .and_then(|ts| ts.parse::<DateTime<Utc>>().ok())
                    .is_some_and(|ts| ts >= cutoff)
        })
        .collect();

    // Sort by completion time (most recent first)
    recent_tasks.sort_by(|a, b| {
        let a_time = a
            .completed_at
            .as_ref()
            .and_then(|ts| ts.parse::<DateTime<Utc>>().ok());
        let b_time = b
            .completed_at
            .as_ref()
            .and_then(|ts| ts.parse::<DateTime<Utc>>().ok());
        b_time.cmp(&a_time)
    });

    if json {
        let output: Vec<serde_json::Value> = recent_tasks
            .iter()
            .map(|task| {
                let last_log = task.log.last().map(|l| {
                    serde_json::json!({
                        "timestamp": l.timestamp,
                        "message": l.message,
                    })
                });
                let mut obj = serde_json::json!({
                    "id": task.id,
                    "title": task.title,
                    "completed_at": task.completed_at,
                    "tags": task.tags,
                });
                if with_artifacts && !task.artifacts.is_empty() {
                    obj["artifacts"] = serde_json::json!(task.artifacts);
                }
                if let Some(log) = last_log {
                    obj["last_log"] = log;
                }
                obj
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if recent_tasks.is_empty() {
        let since_label = since.unwrap_or("24h");
        println!("No tasks completed in the last {}.", since_label);
        return Ok(());
    }

    // Group by tag for display
    let mut by_tag: BTreeMap<String, Vec<&workgraph::graph::Task>> = BTreeMap::new();
    let mut untagged = Vec::new();

    for task in &recent_tasks {
        if task.tags.is_empty() {
            untagged.push(*task);
        } else {
            // Add task to each of its tags (task may appear in multiple groups)
            for tag in &task.tags {
                by_tag.entry(tag.clone()).or_default().push(*task);
            }
        }
    }

    let since_label = since.unwrap_or("24h");
    println!(
        "Recently completed ({} tasks in last {}):\n",
        recent_tasks.len(),
        since_label
    );

    let mut printed_ids = std::collections::HashSet::new();

    for (tag, tasks) in &by_tag {
        println!("  [{}]", tag);
        for task in tasks {
            if !printed_ids.insert(&task.id) {
                continue; // Skip if already printed under another tag
            }
            print_task(task, with_artifacts);
        }
        println!();
    }

    if !untagged.is_empty() {
        println!("  [untagged]");
        for task in &untagged {
            if !printed_ids.insert(&task.id) {
                continue;
            }
            print_task(task, with_artifacts);
        }
        println!();
    }

    Ok(())
}

fn print_task(task: &workgraph::graph::Task, with_artifacts: bool) {
    let completed = task
        .completed_at
        .as_ref()
        .and_then(|ts| ts.parse::<DateTime<Utc>>().ok())
        .map(format_relative)
        .unwrap_or_default();

    println!("    {} — {} ({})", task.id, task.title, completed);

    if with_artifacts && !task.artifacts.is_empty() {
        for artifact in &task.artifacts {
            println!("      artifact: {}", artifact);
        }
    }

    if let Some(last_log) = task.log.last() {
        let msg = if last_log.message.len() > 100 {
            format!("{}...", &last_log.message[..97])
        } else {
            last_log.message.clone()
        };
        println!("      last log: {}", msg);
    }
}

fn format_relative(ts: DateTime<Utc>) -> String {
    let now = Utc::now();
    if ts > now {
        return ts.format("%H:%M").to_string();
    }
    let secs = (now - ts).num_seconds();
    workgraph::format_duration(secs, false) + " ago"
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use tempfile::TempDir;
    use workgraph::graph::{LogEntry, Node, Task, WorkGraph};
    use workgraph::parser::save_graph;

    fn make_done_task(id: &str, title: &str, hours_ago: i64) -> Task {
        let completed = Utc::now() - ChronoDuration::hours(hours_ago);
        Task {
            id: id.to_string(),
            title: title.to_string(),
            status: Status::Done,
            completed_at: Some(completed.to_rfc3339()),
            ..Task::default()
        }
    }

    fn setup_graph(dir: &Path, tasks: Vec<Task>) {
        std::fs::create_dir_all(dir).unwrap();
        let path = super::super::graph_path(dir);
        let mut graph = WorkGraph::new();
        for task in tasks {
            graph.add_node(Node::Task(task));
        }
        save_graph(&graph, &path).unwrap();
    }

    #[test]
    fn test_parse_duration_hours() {
        let d = parse_duration("24h").unwrap();
        assert_eq!(d, Duration::hours(24));
    }

    #[test]
    fn test_parse_duration_days() {
        let d = parse_duration("7d").unwrap();
        assert_eq!(d, Duration::days(7));
    }

    #[test]
    fn test_parse_duration_minutes() {
        let d = parse_duration("30m").unwrap();
        assert_eq!(d, Duration::minutes(30));
    }

    #[test]
    fn test_parse_duration_no_unit_defaults_hours() {
        let d = parse_duration("12").unwrap();
        assert_eq!(d, Duration::hours(12));
    }

    #[test]
    fn test_parse_duration_empty_fails() {
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn test_parse_duration_invalid_number_fails() {
        assert!(parse_duration("abch").is_err());
    }

    #[test]
    fn test_run_no_recent_tasks() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_graph(dir, vec![make_done_task("old", "Old task", 48)]);
        let result = run(dir, Some("24h"), false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_with_recent_tasks() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut task = make_done_task("recent", "Recent task", 2);
        task.tags = vec!["test".to_string()];
        task.artifacts = vec!["output.txt".to_string()];
        task.log = vec![LogEntry {
            timestamp: Utc::now().to_rfc3339(),
            actor: None,
            message: "Done!".to_string(),
        }];
        setup_graph(dir, vec![task]);
        let result = run(dir, Some("24h"), true, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_json_output() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let mut task = make_done_task("recent", "Recent task", 2);
        task.tags = vec!["test".to_string()];
        setup_graph(dir, vec![task]);
        let result = run(dir, Some("24h"), false, true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_default_since() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_graph(dir, vec![make_done_task("recent", "Recent task", 2)]);
        let result = run(dir, None, false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_untagged_tasks() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_graph(dir, vec![make_done_task("t1", "Untagged task", 1)]);
        let result = run(dir, Some("24h"), false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_mixed_recent_and_old() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        setup_graph(
            dir,
            vec![
                make_done_task("recent", "Recent", 1),
                make_done_task("old", "Old", 48),
            ],
        );
        let result = run(dir, Some("24h"), false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_uninitialized() {
        let tmp = TempDir::new().unwrap();
        let result = run(tmp.path(), None, false, false);
        assert!(result.is_err());
    }
}
