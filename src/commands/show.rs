use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use workgraph::graph::{LogEntry, Status, WorkGraph};
use workgraph::parser::load_graph;

use super::graph_path;

/// Blocker info with status
#[derive(Debug, Serialize)]
struct BlockerInfo {
    id: String,
    status: Status,
}

fn is_zero(val: &u32) -> bool {
    *val == 0
}

/// JSON output structure for show command
#[derive(Debug, Serialize)]
struct TaskDetails {
    id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    assigned: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skills: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    inputs: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deliverables: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    artifacts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exec: Option<String>,
    blocked_by: Vec<BlockerInfo>,
    blocks: Vec<BlockerInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    not_before: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    log: Vec<LogEntry>,
    #[serde(skip_serializing_if = "is_zero")]
    retry_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_retries: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure_reason: Option<String>,
}

pub fn run(dir: &Path, id: &str, json: bool) -> Result<()> {
    let path = graph_path(dir);

    if !path.exists() {
        anyhow::bail!("Workgraph not initialized. Run 'wg init' first.");
    }

    let graph = load_graph(&path).context("Failed to load graph")?;

    let task = graph
        .get_task(id)
        .ok_or_else(|| anyhow::anyhow!("Task '{}' not found", id))?;

    // Build reverse index to find what this task blocks
    let reverse_index = build_reverse_index(&graph);

    // Get blocker info with statuses
    let blocked_by_info: Vec<BlockerInfo> = task
        .blocked_by
        .iter()
        .map(|blocker_id| {
            let status = graph
                .get_task(blocker_id)
                .map(|t| t.status.clone())
                .unwrap_or(Status::Open);
            BlockerInfo {
                id: blocker_id.clone(),
                status,
            }
        })
        .collect();

    // Get what this task blocks
    let blocks_info: Vec<BlockerInfo> = reverse_index
        .get(id)
        .map(|dependents| {
            dependents
                .iter()
                .map(|dep_id| {
                    let status = graph
                        .get_task(dep_id)
                        .map(|t| t.status.clone())
                        .unwrap_or(Status::Open);
                    BlockerInfo {
                        id: dep_id.clone(),
                        status,
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    let details = TaskDetails {
        id: task.id.clone(),
        title: task.title.clone(),
        description: task.description.clone(),
        status: task.status.clone(),
        assigned: task.assigned.clone(),
        hours: task.estimate.as_ref().and_then(|e| e.hours),
        cost: task.estimate.as_ref().and_then(|e| e.cost),
        tags: task.tags.clone(),
        skills: task.skills.clone(),
        inputs: task.inputs.clone(),
        deliverables: task.deliverables.clone(),
        artifacts: task.artifacts.clone(),
        exec: task.exec.clone(),
        blocked_by: blocked_by_info,
        blocks: blocks_info,
        created_at: task.created_at.clone(),
        started_at: task.started_at.clone(),
        completed_at: task.completed_at.clone(),
        not_before: task.not_before.clone(),
        log: task.log.clone(),
        retry_count: task.retry_count,
        max_retries: task.max_retries,
        failure_reason: task.failure_reason.clone(),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&details)?);
    } else {
        print_human_readable(&details);
    }

    Ok(())
}

fn print_human_readable(details: &TaskDetails) {
    println!("Task: {}", details.id);
    println!("Title: {}", details.title);
    println!("Status: {}", format_status(&details.status));

    if let Some(ref assigned) = details.assigned {
        println!("Assigned: {}", assigned);
    }

    // Failure info
    if details.status == Status::Failed || details.status == Status::Abandoned {
        if let Some(ref reason) = details.failure_reason {
            println!("Failure reason: {}", reason);
        }
    }
    if details.retry_count > 0 {
        let retry_info = match details.max_retries {
            Some(max) => format!("Retry count: {}/{}", details.retry_count, max),
            None => format!("Retry count: {}", details.retry_count),
        };
        println!("{}", retry_info);
    } else if details.max_retries.is_some() {
        println!("Max retries: {}", details.max_retries.unwrap());
    }

    // Description
    if let Some(ref description) = details.description {
        println!();
        println!("Description:");
        for line in description.lines() {
            println!("  {}", line);
        }
    }

    println!();

    // Estimate section
    let has_estimate = details.hours.is_some() || details.cost.is_some();
    if has_estimate {
        let mut parts = Vec::new();
        if let Some(hours) = details.hours {
            parts.push(format!("{}h", hours));
        }
        if let Some(cost) = details.cost {
            parts.push(format!("${}", cost));
        }
        println!("Estimate: {}", parts.join(", "));
    }

    // Tags
    if !details.tags.is_empty() {
        println!("Tags: {}", details.tags.join(", "));
    }

    // Skills
    if !details.skills.is_empty() {
        println!("Skills: {}", details.skills.join(", "));
    }

    // Inputs
    if !details.inputs.is_empty() {
        println!("Inputs: {}", details.inputs.join(", "));
    }

    // Deliverables
    if !details.deliverables.is_empty() {
        println!("Deliverables: {}", details.deliverables.join(", "));
    }

    println!();

    // Blocked by section
    println!("Blocked by:");
    if details.blocked_by.is_empty() {
        println!("  (none)");
    } else {
        for blocker in &details.blocked_by {
            println!("  - {} ({})", blocker.id, format_status(&blocker.status));
        }
    }

    println!();

    // Blocks section
    println!("Blocks:");
    if details.blocks.is_empty() {
        println!("  (none)");
    } else {
        for blocked in &details.blocks {
            println!("  - {} ({})", blocked.id, format_status(&blocked.status));
        }
    }

    println!();

    // Timestamps
    if let Some(ref created) = details.created_at {
        println!("Created: {}", created);
    }
    if let Some(ref started) = details.started_at {
        println!("Started: {}", started);
    }
    if let Some(ref completed) = details.completed_at {
        println!("Completed: {}", completed);
    }
    if let Some(ref not_before) = details.not_before {
        println!("Not before: {}", not_before);
    }

    // Log entries
    if !details.log.is_empty() {
        println!();
        println!("Log:");
        for entry in &details.log {
            let actor_str = entry
                .actor
                .as_ref()
                .map(|a| format!(" [{}]", a))
                .unwrap_or_default();
            println!("  {} {}{}", entry.timestamp, entry.message, actor_str);
        }
    }
}

fn format_status(status: &Status) -> &'static str {
    match status {
        Status::Open => "open",
        Status::InProgress => "in-progress",
        Status::Done => "done",
        Status::Blocked => "blocked",
        Status::Failed => "failed",
        Status::Abandoned => "abandoned",
    }
}

/// Build a reverse index: for each task, find what tasks list it in their `blocked_by`
fn build_reverse_index(graph: &WorkGraph) -> HashMap<String, Vec<String>> {
    let mut index: HashMap<String, Vec<String>> = HashMap::new();

    for task in graph.tasks() {
        for blocker_id in &task.blocked_by {
            index
                .entry(blocker_id.clone())
                .or_default()
                .push(task.id.clone());
        }
    }

    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Node, Task};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: Status::Open,
            assigned: None,
            estimate: None,
            blocks: vec![],
            blocked_by: vec![],
            requires: vec![],
            tags: vec![],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            not_before: None,
            created_at: None,
            started_at: None,
            completed_at: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
        }
    }

    #[test]
    fn test_build_reverse_index() {
        let mut graph = WorkGraph::new();

        let t1 = make_task("t1", "Task 1");
        let mut t2 = make_task("t2", "Task 2");
        t2.blocked_by = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.blocked_by = vec!["t1".to_string()];

        graph.add_node(Node::Task(t1));
        graph.add_node(Node::Task(t2));
        graph.add_node(Node::Task(t3));

        let index = build_reverse_index(&graph);
        let dependents = index.get("t1").unwrap();
        assert_eq!(dependents.len(), 2);
        assert!(dependents.contains(&"t2".to_string()));
        assert!(dependents.contains(&"t3".to_string()));
    }

    #[test]
    fn test_format_status() {
        assert_eq!(format_status(&Status::Open), "open");
        assert_eq!(format_status(&Status::InProgress), "in-progress");
        assert_eq!(format_status(&Status::Done), "done");
        assert_eq!(format_status(&Status::Blocked), "blocked");
    }

    #[test]
    fn test_task_details_serialization() {
        let details = TaskDetails {
            id: "t1".to_string(),
            title: "Test Task".to_string(),
            description: Some("Test description".to_string()),
            status: Status::InProgress,
            assigned: Some("agent-1".to_string()),
            hours: Some(2.0),
            cost: Some(200.0),
            tags: vec!["test".to_string()],
            skills: vec![],
            inputs: vec![],
            deliverables: vec![],
            artifacts: vec![],
            exec: None,
            blocked_by: vec![],
            blocks: vec![BlockerInfo {
                id: "t2".to_string(),
                status: Status::Open,
            }],
            created_at: Some("2026-01-20T15:35:50+00:00".to_string()),
            started_at: Some("2026-01-20T16:30:00+00:00".to_string()),
            completed_at: None,
            not_before: None,
            log: vec![],
            retry_count: 0,
            max_retries: None,
            failure_reason: None,
        };

        let json = serde_json::to_string(&details).unwrap();
        assert!(json.contains("\"id\":\"t1\""));
        assert!(json.contains("\"status\":\"in-progress\""));
        assert!(json.contains("\"assigned\":\"agent-1\""));
        assert!(json.contains("\"description\":\"Test description\""));
    }
}
