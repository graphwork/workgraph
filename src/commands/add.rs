use anyhow::{Context, Result};
use chrono::Utc;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use workgraph::graph::{Estimate, Node, Status, Task};
use workgraph::parser::load_graph;

use super::graph_path;

pub fn run(
    dir: &Path,
    title: &str,
    id: Option<&str>,
    description: Option<&str>,
    blocked_by: &[String],
    assign: Option<&str>,
    hours: Option<f64>,
    cost: Option<f64>,
    tags: &[String],
    skills: &[String],
    inputs: &[String],
    deliverables: &[String],
    max_retries: Option<u32>,
) -> Result<()> {
    let path = graph_path(dir);

    // Load existing graph to check for ID conflicts
    let graph = if path.exists() {
        load_graph(&path).context("Failed to load graph")?
    } else {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    };

    // Generate ID if not provided
    let task_id = match id {
        Some(id) => {
            if graph.get_node(id).is_some() {
                anyhow::bail!("Task with ID '{}' already exists", id);
            }
            id.to_string()
        }
        None => generate_id(title, &graph),
    };

    let estimate = if hours.is_some() || cost.is_some() {
        Some(Estimate { hours, cost })
    } else {
        None
    };

    let task = Task {
        id: task_id.clone(),
        title: title.to_string(),
        description: description.map(String::from),
        status: Status::Open,
        assigned: assign.map(String::from),
        estimate,
        blocks: vec![],
        blocked_by: blocked_by.to_vec(),
        requires: vec![],
        tags: tags.to_vec(),
        skills: skills.to_vec(),
        inputs: inputs.to_vec(),
        deliverables: deliverables.to_vec(),
        artifacts: vec![],
        exec: None,
        not_before: None,
        created_at: Some(Utc::now().to_rfc3339()),
        started_at: None,
        completed_at: None,
        log: vec![],
        retry_count: 0,
        max_retries,
        failure_reason: None,
    };

    // Append to file
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("Failed to open graph.jsonl")?;

    let json = serde_json::to_string(&Node::Task(task)).context("Failed to serialize task")?;
    writeln!(file, "{}", json).context("Failed to write task")?;

    println!("Added task: {} ({})", title, task_id);
    Ok(())
}

fn generate_id(title: &str, graph: &workgraph::WorkGraph) -> String {
    // Generate a slug from the title
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join("-");

    let base_id = if slug.is_empty() { "task".to_string() } else { slug };

    // Ensure uniqueness
    if graph.get_node(&base_id).is_none() {
        return base_id;
    }

    for i in 2..1000 {
        let candidate = format!("{}-{}", base_id, i);
        if graph.get_node(&candidate).is_none() {
            return candidate;
        }
    }

    // Fallback to timestamp
    format!("task-{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs())
}
