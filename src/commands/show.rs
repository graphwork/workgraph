use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::Path;
use workgraph::graph::{LogEntry, LoopEdge, LoopGuard, Status};
use workgraph::query::build_reverse_index;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verify: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    loops_to: Vec<LoopEdge>,
    #[serde(skip_serializing_if = "is_zero")]
    loop_iteration: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    ready_after: Option<String>,
    #[serde(default, skip_serializing_if = "is_not_paused")]
    paused: bool,
}

fn is_not_paused(val: &bool) -> bool {
    !*val
}

pub fn run(dir: &Path, id: &str, json: bool) -> Result<()> {
    let (graph, _path) = super::load_workgraph(dir)?;

    let task = graph.get_task_or_err(id)?;

    // Build reverse index to find what this task blocks
    let reverse_index = build_reverse_index(&graph);

    // Get blocker info with statuses (supports cross-repo peer:task-id references)
    let blocked_by_info: Vec<BlockerInfo> = task
        .blocked_by
        .iter()
        .map(|blocker_id| {
            if let Some((peer_name, remote_task_id)) =
                workgraph::federation::parse_remote_ref(blocker_id)
            {
                // Cross-repo dependency — resolve via federation
                let remote = workgraph::federation::resolve_remote_task_status(
                    peer_name,
                    remote_task_id,
                    dir,
                );
                BlockerInfo {
                    id: blocker_id.clone(),
                    status: remote.status,
                }
            } else {
                let status = match graph.get_task(blocker_id) {
                    Some(t) => t.status,
                    None => {
                        eprintln!(
                            "Warning: blocker '{}' referenced by '{}' not found in graph",
                            blocker_id, id
                        );
                        Status::Open
                    }
                };
                BlockerInfo {
                    id: blocker_id.clone(),
                    status,
                }
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
                        .map(|t| t.status)
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
        status: task.status,
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
        model: task.model.clone(),
        verify: task.verify.clone(),
        agent: task.agent.clone(),
        loops_to: task.loops_to.clone(),
        loop_iteration: task.loop_iteration,
        ready_after: task.ready_after.clone(),
        paused: task.paused,
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
    if details.paused {
        println!("Status: {} (PAUSED)", details.status);
    } else {
        println!("Status: {}", details.status);
    }

    if let Some(ref assigned) = details.assigned {
        println!("Assigned: {}", assigned);
    }
    if let Some(ref agent) = details.agent {
        println!("Agent: {}", agent);
    }

    // Failure info
    if (details.status == Status::Failed || details.status == Status::Abandoned)
        && let Some(ref reason) = details.failure_reason
    {
        println!("Failure reason: {}", reason);
    }
    if details.retry_count > 0 {
        let retry_info = match details.max_retries {
            Some(max) => format!("Retry count: {}/{}", details.retry_count, max),
            None => format!("Retry count: {}", details.retry_count),
        };
        println!("{}", retry_info);
    } else if let Some(max) = details.max_retries {
        println!("Max retries: {}", max);
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
            println!("  - {} ({})", blocker.id, blocker.status);
        }
    }

    println!();

    // Blocks section
    println!("Blocks:");
    if details.blocks.is_empty() {
        println!("  (none)");
    } else {
        for blocked in &details.blocks {
            println!("  - {} ({})", blocked.id, blocked.status);
        }
    }

    // Loop edges
    if !details.loops_to.is_empty() || details.loop_iteration > 0 {
        println!();
        println!("Loops:");
        for edge in &details.loops_to {
            let guard_str = match &edge.guard {
                Some(LoopGuard::TaskStatus { task, status }) => {
                    format!(", guard: task:{}={}", task, status)
                }
                Some(LoopGuard::IterationLessThan(n)) => {
                    format!(", guard: iteration<{}", n)
                }
                Some(LoopGuard::Always) => ", guard: always".to_string(),
                None => String::new(),
            };
            let delay_str = match &edge.delay {
                Some(d) => format!(", delay: {}", d),
                None => String::new(),
            };
            println!(
                "  → {} (max: {}{}{})",
                edge.target, edge.max_iterations, guard_str, delay_str,
            );
        }
        if details.loop_iteration > 0 {
            println!("  Current iteration: {}", details.loop_iteration);
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
    if let Some(ref ready_after) = details.ready_after {
        println!(
            "Ready after: {}{}",
            ready_after,
            format_countdown(ready_after)
        );
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

/// Format a timestamp as a countdown string if it's in the future, or "(elapsed)" if in the past.
fn format_countdown(timestamp: &str) -> String {
    let Ok(ts) = timestamp.parse::<DateTime<Utc>>() else {
        return String::new();
    };
    let now = Utc::now();
    if ts <= now {
        return " (elapsed)".to_string();
    }
    let secs = (ts - now).num_seconds();
    if secs < 60 {
        format!(" (in {}s)", secs)
    } else if secs < 3600 {
        format!(" (in {}m {}s)", secs / 60, secs % 60)
    } else if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!(" (in {}h {}m)", h, m)
    } else {
        let d = secs / 86400;
        let h = (secs % 86400) / 3600;
        format!(" (in {}d {}h)", d, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workgraph::graph::{Node, Task, WorkGraph};

    fn make_task(id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            title: title.to_string(),
            ..Task::default()
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
    fn test_status_display() {
        assert_eq!(Status::Open.to_string(), "open");
        assert_eq!(Status::InProgress.to_string(), "in-progress");
        assert_eq!(Status::Done.to_string(), "done");
        assert_eq!(Status::Blocked.to_string(), "blocked");
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
            model: None,
            verify: None,
            agent: None,
            loops_to: vec![],
            loop_iteration: 0,
            ready_after: None,
            paused: false,
        };

        let json = serde_json::to_string(&details).unwrap();
        assert!(json.contains("\"id\":\"t1\""));
        assert!(json.contains("\"status\":\"in-progress\""));
        assert!(json.contains("\"assigned\":\"agent-1\""));
        assert!(json.contains("\"description\":\"Test description\""));
    }

    #[test]
    fn test_status_display_all_variants() {
        assert_eq!(Status::Open.to_string(), "open");
        assert_eq!(Status::InProgress.to_string(), "in-progress");
        assert_eq!(Status::Done.to_string(), "done");
        assert_eq!(Status::Blocked.to_string(), "blocked");
        assert_eq!(Status::Failed.to_string(), "failed");
        assert_eq!(Status::Abandoned.to_string(), "abandoned");
    }

    #[test]
    fn test_format_countdown_invalid_timestamp() {
        let result = format_countdown("not-a-timestamp");
        assert_eq!(result, "");
    }

    #[test]
    fn test_format_countdown_past_timestamp() {
        let past = "2020-01-01T00:00:00+00:00";
        let result = format_countdown(past);
        assert_eq!(result, " (elapsed)");
    }

    #[test]
    fn test_run_nonexistent_task() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let graph = WorkGraph::new();
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "no-such-task", false);
        assert!(result.is_err());
    }

    #[test]
    fn test_run_basic_task() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_json_output() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "Test task")));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_task_with_orphan_blocker() {
        // A task references a blocker that doesn't exist in the graph
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Task with ghost blocker");
        task.blocked_by = vec!["nonexistent".to_string()];
        graph.add_node(Node::Task(task));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        // Should succeed (not crash), blocker defaults to Status::Open with a warning
        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_task_with_orphan_blocker_json() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();
        let mut task = make_task("t1", "Task with ghost blocker");
        task.blocked_by = vec!["ghost".to_string()];
        graph.add_node(Node::Task(task));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_run_no_graph_file() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_err());
    }
}
