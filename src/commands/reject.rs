//! Reject command - send a pending-review task back for rework
//!
//! Used by reviewers to reject work submitted by agents.
//! Sets the task back to Open so it can be claimed and reworked.

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

pub fn run(dir: &Path, task_id: &str, reason: Option<&str>, actor: Option<&str>) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(task_id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", task_id))?;

    // Only allow reject from PendingReview
    if task.status != Status::PendingReview {
        anyhow::bail!(
            "Cannot reject task '{}': status is {:?}, expected PendingReview",
            task_id,
            task.status
        );
    }

    // Set status back to Open for rework
    task.status = Status::Open;
    task.assigned = None;
    task.retry_count += 1;

    // Add log entry
    let message = match reason {
        Some(r) => format!("Work rejected: {}", r),
        None => "Work rejected (no reason given)".to_string(),
    };
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: actor.map(String::from),
        message,
    });

    save_graph(&graph, &path).context("Failed to save graph")?;

    println!("Rejected task '{}' - returned to open for rework", task_id);

    Ok(())
}
