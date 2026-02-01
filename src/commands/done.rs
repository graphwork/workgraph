use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::Status;
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

pub fn run(dir: &Path, id: &str) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;

    if task.status == Status::Done {
        println!("Task '{}' is already done", id);
        return Ok(());
    }

    // Verified tasks must use submit -> approve workflow
    if task.verify.is_some() {
        anyhow::bail!(
            "Task '{}' requires verification. Use 'wg submit {}' instead of 'wg done'.\n\
             After submission, a reviewer must use 'wg approve {}' to complete it.",
            id, id, id
        );
    }

    task.status = Status::Done;
    task.completed_at = Some(Utc::now().to_rfc3339());

    save_graph(&graph, &path).context("Failed to save graph")?;

    println!("Marked '{}' as done", id);
    Ok(())
}
