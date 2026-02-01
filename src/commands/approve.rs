//! Approve command - mark a pending-review task as done
//!
//! Used by reviewers to approve work submitted by agents.

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

pub fn run(dir: &Path, task_id: &str, actor: Option<&str>) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Only allow approve from PendingReview
    if task.status != Status::PendingReview {
        anyhow::bail!(
            "Cannot approve task '{}': status is {:?}, expected PendingReview",
            task_id,
            task.status
        );
    }

    // Set status to Done
    task.status = Status::Done;
    task.completed_at = Some(Utc::now().to_rfc3339());

    // Add log entry
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: actor.map(String::from),
        message: "Work approved and marked done".to_string(),
    });

    save_graph(&graph, &path).context("Failed to save graph")?;

    println!("Approved task '{}' - now done", task_id);

    Ok(())
}
