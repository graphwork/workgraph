//! Approve command - mark a pending-review task as done
//!
//! Used by reviewers to approve work submitted by agents.

use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::{evaluate_loop_edges, LogEntry, Status};
use workgraph::parser::{load_graph, save_graph};
use workgraph::query;

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

    // Check for unresolved blockers
    let blockers = query::blocked_by(&graph, task_id);
    if !blockers.is_empty() {
        let blocker_list: Vec<String> = blockers
            .iter()
            .map(|t| format!("  - {} ({}): {:?}", t.id, t.title, t.status))
            .collect();
        anyhow::bail!(
            "Cannot approve task '{}': blocked by {} unresolved task(s):\n{}",
            task_id,
            blockers.len(),
            blocker_list.join("\n")
        );
    }

    // Re-acquire mutable reference after immutable borrow
    let task = graph.get_task_mut(task_id).unwrap();

    // Set status to Done
    task.status = Status::Done;
    task.completed_at = Some(Utc::now().to_rfc3339());

    // Add log entry
    task.log.push(LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: actor.map(String::from),
        message: "Work approved and marked done".to_string(),
    });

    // Evaluate loop edges: re-activate upstream tasks if conditions are met
    let id_owned = task_id.to_string();
    let reactivated = evaluate_loop_edges(&mut graph, &id_owned);

    save_graph(&graph, &path).context("Failed to save graph")?;
    super::notify_graph_changed(dir);

    println!("Approved task '{}' - now done", task_id);

    for tid in &reactivated {
        println!("  Loop: re-activated '{}'", tid);
    }

    Ok(())
}
