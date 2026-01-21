use anyhow::{Context, Result};
use chrono::Utc;
use std::path::Path;
use workgraph::graph::LogEntry;
use workgraph::parser::{load_graph, save_graph};

use super::graph_path;

/// Add a log entry to a task
pub fn run_add(dir: &Path, id: &str, message: &str, actor: Option<&str>) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let mut graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task_mut(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;

    let entry = LogEntry {
        timestamp: Utc::now().to_rfc3339(),
        actor: actor.map(String::from),
        message: message.to_string(),
    };

    task.log.push(entry);

    save_graph(&graph, &path).context("Failed to save graph")?;

    let actor_str = actor.map(|a| format!(" ({})", a)).unwrap_or_default();
    println!("Added log entry to '{}'{}", id, actor_str);
    Ok(())
}

/// List log entries for a task
pub fn run_list(dir: &Path, id: &str, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;

    if json {
        println!("{}", serde_json::to_string_pretty(&task.log)?);
        return Ok(());
    }

    if task.log.is_empty() {
        println!("No log entries for task '{}'", id);
        return Ok(());
    }

    println!("Log entries for '{}' ({}):", id, task.title);
    println!();

    for entry in &task.log {
        let actor_str = entry.actor.as_ref().map(|a| format!(" [{}]", a)).unwrap_or_default();
        println!("  {} {}", entry.timestamp, actor_str);
        println!("    {}", entry.message);
        println!();
    }

    Ok(())
}
