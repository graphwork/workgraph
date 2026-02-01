use anyhow::{Context, Result};
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
            .map(|t| serde_json::json!({
                "id": t.id,
                "title": t.title,
                "status": t.status,
                "assigned": t.assigned,
                "blocked_by": t.blocked_by,
            }))
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
                println!("{} {} - {}", status, task.id, task.title);
            }
        }
    }

    Ok(())
}
