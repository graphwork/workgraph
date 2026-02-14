use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::agency::capture_task_output;
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
        anyhow::bail!("Task '{}' is already done and cannot be marked as failed", id);
    }

    if task.status == Status::Abandoned {
        anyhow::bail!("Task '{}' is already abandoned", id);
    }

    if task.status == Status::Failed {
        println!("Task '{}' is already failed (retry_count: {})", id, task.retry_count);
        return Ok(());
    }

    task.status = Status::Failed;
    task.retry_count += 1;
    task.failure_reason = reason.map(String::from);

    let log_message = match reason {
        Some(r) => format!("Task marked as failed: {}", r),
        None => "Task marked as failed".to_string(),
    };
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: task.assigned.clone(),
        message: log_message,
    });

    // Extract values we need for printing before saving
    let retry_count = task.retry_count;
    let max_retries = task.max_retries;

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    let reason_msg = reason.map(|r| format!(" ({})", r)).unwrap_or_default();
    println!(
        "Marked '{}' as failed{} (retry #{})",
        id, reason_msg, retry_count
    );

    // Show retry info if max_retries is set
    if let Some(max) = max_retries {
        if retry_count >= max {
            println!("  Warning: Max retries ({}) reached. Consider abandoning or increasing limit.", max);
        } else {
            println!("  Retries remaining: {}", max - retry_count);
        }
    }

    // Capture task output (git diff, artifacts, log) for evaluation.
    // Failed tasks are also evaluated when auto_evaluate is enabled — there is
    // useful signal in what kinds of tasks cause which agents to fail.
    if let Some(task) = graph.get_task(id) {
        match capture_task_output(dir, task) {
            Ok(output_dir) => {
                eprintln!("Output captured to {}", output_dir.display());
            }
            Err(e) => {
                eprintln!("Warning: output capture failed: {}", e);
            }
        }
    }

    Ok(())
}
