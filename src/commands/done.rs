use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::agency::capture_task_output;
use workgraph::graph::{evaluate_loop_edges, LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query;

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

    // Check for unresolved blockers
    let blockers = query::blocked_by(&graph, id);
    if !blockers.is_empty() {
        let blocker_list: Vec<String> = blockers
            .iter()
            .map(|t| format!("  - {} ({}): {:?}", t.id, t.title, t.status))
            .collect();
        anyhow::bail!(
            "Cannot mark '{}' as done: blocked by {} unresolved task(s):\n{}",
            id,
            blockers.len(),
            blocker_list.join("\n")
        );
    }

    // Re-acquire mutable reference after immutable borrow
    let task = graph.get_task_mut(id).unwrap();

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

    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: task.assigned.clone(),
        message: "Task marked as done".to_string(),
    });

    // Evaluate loop edges: re-activate upstream tasks if conditions are met
    let id_owned = id.to_string();
    let reactivated = evaluate_loop_edges(&mut graph, &id_owned);

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    println!("Marked '{}' as done", id);

    for task_id in &reactivated {
        println!("  Loop: re-activated '{}'", task_id);
    }

    // Capture task output (git diff, artifacts, log) for evaluation.
    // When auto_evaluate is enabled, the coordinator creates an evaluation task
    // in the graph that becomes ready once this task is done; the captured output
    // feeds that evaluator.
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
