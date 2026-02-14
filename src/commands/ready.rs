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
            let has_future_ready_after = task.ready_after.as_ref().map_or(false, |ra| {
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
            .map(|t| serde_json::json!({
                "id": t.id,
                "title": t.title,
                "assigned": t.assigned,
                "estimate": t.estimate,
                "ready": true,
            }))
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
    } else {
        if ready.is_empty() && waiting.is_empty() {
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
