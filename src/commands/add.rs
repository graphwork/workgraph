use anyhow::{Context, Result};
use chrono::Utc;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use workgraph::graph::{Estimate, LoopEdge, Node, Status, Task, parse_delay};
use workgraph::parser::load_graph;

use super::graph_path;

/// Parse a guard expression string into a LoopGuard.
/// Formats: 'task:<id>=<status>' or 'always'
pub fn parse_guard_expr(expr: &str) -> Result<workgraph::graph::LoopGuard> {
    let expr = expr.trim();
    if expr.eq_ignore_ascii_case("always") {
        return Ok(workgraph::graph::LoopGuard::Always);
    }
    if let Some(rest) = expr.strip_prefix("task:") {
        if let Some((task_id, status_str)) = rest.split_once('=') {
            let status = match status_str.to_lowercase().as_str() {
                "open" => Status::Open,
                "in-progress" => Status::InProgress,
                "done" => Status::Done,
                "blocked" => Status::Blocked,
                "failed" => Status::Failed,
                "abandoned" => Status::Abandoned,
                "pending-review" => Status::PendingReview,
                _ => anyhow::bail!("Unknown status '{}' in guard expression", status_str),
            };
            return Ok(workgraph::graph::LoopGuard::TaskStatus {
                task: task_id.to_string(),
                status,
            });
        }
        anyhow::bail!("Invalid guard format. Expected 'task:<id>=<status>', got '{}'", expr);
    }
    anyhow::bail!("Invalid guard expression '{}'. Expected 'task:<id>=<status>' or 'always'", expr);
}

#[allow(clippy::too_many_arguments)]
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
    model: Option<&str>,
    verify: Option<&str>,
    loops_to: Option<&str>,
    loop_max: Option<u32>,
    loop_guard: Option<&str>,
    loop_delay: Option<&str>,
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

    // Build loop edges if --loops-to specified
    let loops_to_edges = if let Some(target) = loops_to {
        let max_iterations = loop_max
            .ok_or_else(|| anyhow::anyhow!("--loop-max is required when using --loops-to"))?;
        let guard = match loop_guard {
            Some(expr) => Some(parse_guard_expr(expr)?),
            None => None,
        };
        let delay = match loop_delay {
            Some(d) => {
                // Validate the delay parses correctly
                parse_delay(d)
                    .ok_or_else(|| anyhow::anyhow!("Invalid delay '{}'. Use format: 30s, 5m, 1h, 24h, 7d", d))?;
                Some(d.to_string())
            }
            None => None,
        };
        vec![LoopEdge {
            target: target.to_string(),
            guard,
            max_iterations,
            delay,
        }]
    } else {
        if loop_max.is_some() || loop_guard.is_some() || loop_delay.is_some() {
            anyhow::bail!("--loop-max, --loop-guard, and --loop-delay require --loops-to");
        }
        vec![]
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
        model: model.map(String::from),
        verify: verify.map(String::from),
        agent: None,
        loops_to: loops_to_edges,
        loop_iteration: 0,
        ready_after: None,
    };

    // Append to file
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .context("Failed to open graph.jsonl")?;

    let json = serde_json::to_string(&Node::Task(task)).context("Failed to serialize task")?;
    writeln!(file, "{}", json).context("Failed to write task")?;
    super::notify_graph_changed(dir);

    println!("Added task: {} ({})", title, task_id);
    if loops_to.is_some() {
        println!("  Loop edge: → {} (max {} iterations)", loops_to.unwrap(), loop_max.unwrap());
    }
    super::print_service_hint(dir);
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
