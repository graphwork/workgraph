use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

pub fn run(dir: &Path, id: &str, reason: Option<&str>) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;

    if task.status == Status::Done {
        anyhow::bail!("Task '{}' is already done and cannot be abandoned", id);
    }

    if task.status == Status::Abandoned {
        println!("Task '{}' is already abandoned", id);
        return Ok(());
    }

    task.status = Status::Abandoned;
    task.failure_reason = reason.map(String::from);

    let log_message = match reason {
        Some(r) => format!("Task abandoned: {}", r),
        None => "Task abandoned".to_string(),
    };
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: task.assigned.clone(),
        message: log_message,
    });

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    let reason_msg = reason.map(|r| format!(" ({})", r)).unwrap_or_default();
    println!("Marked '{}' as abandoned{}", id, reason_msg);

    Ok(())
}
