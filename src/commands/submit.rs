//! Submit command - mark work complete, awaiting review
//!
//! For tasks with --verify set, agents must use submit instead of done.
//! This sets the task to PendingReview status.

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

    // Only allow submit from InProgress
    if task.status != Status::InProgress {
        anyhow::bail!(
            "Cannot submit task '{}': status is {:?}, expected InProgress",
            task_id,
            task.status
        );
    }

    // Set status to PendingReview
    task.status = Status::PendingReview;

    // Add log entry
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: actor.map(String::from),
        message: "Work submitted for review".to_string(),
    });

    save_graph(&graph, &path).context("Failed to save graph")?;

    println!("Submitted task '{}' for review", task_id);
    if let Some(ref verify) = graph.get_task(task_id).and_then(|t| t.verify.clone()) {
        println!("Verification criteria: {}", verify);
    }

    Ok(())
}
