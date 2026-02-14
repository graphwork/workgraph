use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::path::Path;
use workgraph::graph::Status;
use workgraph::parser::load_graph;

use super::graph_path;

pub fn run(dir: &Path, status_filter: Option<&str>, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let status_filter: Option<Status> = match status_filter {
        Some("open") => Some(Status::Open),
        Some("done") => Some(Status::Done),
        Some("in-progress") => Some(Status::InProgress),
        Some("blocked") => Some(Status::Blocked),
        Some(s) => anyhow::bail!("Unknown status: {}", s),
        None => None,
    };

    let tasks: Vec<_> = graph
        .tasks()
        .filter(|t| status_filter.as_ref().map_or(true, |s| &t.status == s))
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
                    "blocked_by": t.blocked_by,
                });
                if let Some(ref ra) = t.ready_after {
                    obj["ready_after"] = serde_json::json!(ra);
                }
                obj
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        if tasks.is_empty() {
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
                    Status::PendingReview => "[R]",
                };
                let delay_str = format_ready_after_hint(task.ready_after.as_deref());
                println!("{} {} - {}{}", status, task.id, task.title, delay_str);
            }
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
