use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::path::Path;
use workgraph::graph::{
    CycleConfig, LogEntry, LoopGuard, Status, TokenUsage, format_tokens, parse_token_usage_live,
};
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
    after: Vec<BlockerInfo>,
    before: Vec<BlockerInfo>,
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
    #[serde(skip_serializing_if = "is_zero")]
    loop_iteration: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_iteration_completed_at: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    cycle_failure_restarts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    cycle_config: Option<CycleConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ready_after: Option<String>,
    #[serde(default, skip_serializing_if = "is_not_paused")]
    paused: bool,
    #[serde(skip_serializing_if = "is_default_visibility")]
    visibility: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exec_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token_usage: Option<TokenUsage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_condition: Option<workgraph::graph::WaitSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    verify_failures: u32,
    #[serde(default, skip_serializing_if = "is_zero")]
    resurrection_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_resurrected_at: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    superseded_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supersedes: Option<String>,
}

fn is_default_visibility(val: &str) -> bool {
    val == "internal"
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
    let after_info: Vec<BlockerInfo> = task
        .after
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
    let before_info: Vec<BlockerInfo> = reverse_index
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

    // Resolve token usage: stored data first, then live data for in-progress tasks.
    // Check output.log first (works for both Claude CLI and native executor formats),
    // then fall back to stream.jsonl (native executor writes usage there directly).
    let token_usage = task.token_usage.clone().or_else(|| {
        let agent_id = task.assigned.as_deref()?;
        let agent_dir = dir.join("agents").join(agent_id);
        // Try output.log (handles both Claude CLI and native executor formats)
        let log_path = agent_dir.join("output.log");
        if let Some(usage) = parse_token_usage_live(&log_path) {
            return Some(usage);
        }
        // Fallback: read stream.jsonl (native executor writes usage there directly)
        let stream_path = agent_dir.join(workgraph::stream_event::STREAM_FILE_NAME);
        if stream_path.exists()
            && let Ok((events, _)) = workgraph::stream_event::read_stream_events(&stream_path, 0)
            && !events.is_empty()
        {
            let mut state = workgraph::stream_event::AgentStreamState::default();
            state.ingest(&events, 0);
            let usage = state.to_token_usage();
            if usage.input_tokens > 0 || usage.output_tokens > 0 {
                return Some(usage);
            }
        }
        None
    });

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
        after: after_info,
        before: before_info,
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
        loop_iteration: task.loop_iteration,
        last_iteration_completed_at: task.last_iteration_completed_at.clone(),
        cycle_failure_restarts: task.cycle_failure_restarts,
        cycle_config: task.cycle_config.clone(),
        ready_after: task.ready_after.clone(),
        paused: task.paused,
        visibility: task.visibility.clone(),
        context_scope: task.context_scope.clone(),
        exec_mode: task.exec_mode.clone(),
        token_usage,
        session_id: task.session_id.clone(),
        wait_condition: task.wait_condition.clone(),
        checkpoint: task.checkpoint.clone(),
        verify_failures: task.verify_failures,
        resurrection_count: task.resurrection_count,
        last_resurrected_at: task.last_resurrected_at.clone(),
        superseded_by: task.superseded_by.clone(),
        supersedes: task.supersedes.clone(),
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

    if details.visibility != "internal" {
        println!("Visibility: {}", details.visibility);
    }

    if let Some(ref scope) = details.context_scope {
        println!("Context scope: {}", scope);
    }

    if let Some(ref mode) = details.exec_mode {
        println!("Exec mode: {}", mode);
    }

    if let Some(ref assigned) = details.assigned {
        println!("Assigned: {}", assigned);
    }
    if let Some(ref agent) = details.agent {
        println!("Agent: {}", agent);
    }

    // Verify status
    if details.verify.is_some() || details.verify_failures > 0 {
        println!();
        println!("Verify:");
        if let Some(ref cmd) = details.verify {
            println!("  Command: {}", cmd);
        }
        if details.verify_failures > 0 {
            let breaker_tripped = details.status == Status::Failed
                && details
                    .log
                    .iter()
                    .any(|e| e.actor.as_deref() == Some("verify-circuit-breaker"));
            println!("  Failures: {}", details.verify_failures);
            if breaker_tripped {
                println!("  Circuit breaker: \x1b[31mTRIPPED\x1b[0m");
            }
            // Show last verify error from log
            if let Some(last_err) = details
                .log
                .iter()
                .rev()
                .find(|e| e.actor.as_deref() == Some("verify"))
            {
                // Extract stderr from the log message
                if let Some(stderr_pos) = last_err.message.find("\nstderr: ") {
                    let stderr = &last_err.message[stderr_pos + 9..];
                    // Trim at next section or end
                    let stderr = stderr
                        .find("\nstdout: ")
                        .map(|p| &stderr[..p])
                        .unwrap_or(stderr);
                    println!("  Last error: {}", stderr.trim());
                }
            }
        }
    }

    // Failure info
    if (details.status == Status::Failed || details.status == Status::Abandoned)
        && let Some(ref reason) = details.failure_reason
    {
        println!("Failure reason: {}", reason);
    }
    if !details.superseded_by.is_empty() {
        println!("Superseded by: {}", details.superseded_by.join(", "));
    }
    if let Some(ref sup) = details.supersedes {
        println!("Supersedes: {}", sup);
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

    // After section
    println!("After:");
    if details.after.is_empty() {
        println!("  (none)");
    } else {
        for blocker in &details.after {
            println!("  - {} ({})", blocker.id, blocker.status);
        }
    }

    println!();

    // Blocks section
    println!("Before:");
    if details.before.is_empty() {
        println!("  (none)");
    } else {
        for blocked in &details.before {
            println!("  - {} ({})", blocked.id, blocked.status);
        }
    }

    // Cycle config
    if let Some(ref cc) = details.cycle_config {
        println!();
        println!("Cycle config (header):");
        println!("  Max iterations: {}", cc.max_iterations);
        if let Some(ref guard) = cc.guard {
            let guard_str = match guard {
                LoopGuard::TaskStatus { task, status } => {
                    format!("task:{}={}", task, status)
                }
                LoopGuard::IterationLessThan(n) => format!("iteration<{}", n),
                LoopGuard::Always => "always".to_string(),
            };
            println!("  Guard: {}", guard_str);
        }
        if let Some(ref delay) = cc.delay {
            println!("  Delay: {}", delay);
        }
        if cc.no_converge {
            println!("  No-converge: true (all iterations forced)");
        }
        // Display 1-based iteration: loop_iteration=0 is "iteration 1/max"
        println!(
            "  Current iteration: {}/{}",
            details.loop_iteration + 1,
            cc.max_iterations
        );

        // Cycle timing: last iteration completed
        if let Some(ref last_ts) = details.last_iteration_completed_at {
            if let Ok(parsed) = last_ts.parse::<DateTime<Utc>>() {
                let ago = Utc::now().signed_duration_since(parsed).num_seconds();
                println!(
                    "  Last iteration completed: {} ({} ago)",
                    last_ts,
                    workgraph::format_duration(ago, true)
                );
            } else {
                println!("  Last iteration completed: {}", last_ts);
            }
        }

        // Next due: compute from ready_after or last_iteration_completed_at + delay
        let next_due = details.ready_after.clone().or_else(|| {
            let delay_secs = cc
                .delay
                .as_ref()
                .and_then(|d| workgraph::graph::parse_delay(d))?;
            let last_ts = details
                .last_iteration_completed_at
                .as_ref()?
                .parse::<DateTime<Utc>>()
                .ok()?;
            let next = last_ts + chrono::Duration::seconds(delay_secs as i64);
            Some(next.to_rfc3339())
        });
        if let Some(ref next_ts) = next_due
            && let Ok(parsed) = next_ts.parse::<DateTime<Utc>>()
        {
            let now = Utc::now();
            if parsed > now {
                let secs = (parsed - now).num_seconds();
                println!(
                    "  Next iteration due: in {}",
                    workgraph::format_duration(secs, true)
                );
            } else {
                println!("  Next iteration due: ready now");
            }
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
        println!("Not before: {}{}", not_before, format_countdown(not_before));
    }
    if let Some(ref ready_after) = details.ready_after {
        println!(
            "Ready after: {}{}",
            ready_after,
            format_countdown(ready_after)
        );
    }

    // Token usage
    if let Some(ref usage) = details.token_usage {
        println!();
        let novel_in = usage.input_tokens + usage.cache_creation_input_tokens;
        if usage.cache_read_input_tokens > 0 {
            println!(
                "Tokens: {}/{} (in/out) +{} cached",
                format_tokens(novel_in),
                format_tokens(usage.output_tokens),
                format_tokens(usage.cache_read_input_tokens)
            );
        } else {
            println!(
                "Tokens: {}/{} (in/out)",
                format_tokens(novel_in),
                format_tokens(usage.output_tokens)
            );
        }
        if usage.cost_usd > 0.0 {
            println!("Cost: ${:.2}", usage.cost_usd);
        }
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
        t2.after = vec!["t1".to_string()];
        let mut t3 = make_task("t3", "Task 3");
        t3.after = vec!["t1".to_string()];

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
            after: vec![],
            before: vec![BlockerInfo {
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
            loop_iteration: 0,
            last_iteration_completed_at: None,
            cycle_failure_restarts: 0,
            ready_after: None,
            paused: false,
            visibility: "internal".to_string(),
            context_scope: None,
            exec_mode: None,
            cycle_config: None,
            token_usage: None,
            session_id: None,
            wait_condition: None,
            checkpoint: None,
            verify_failures: 0,
            resurrection_count: 0,
            last_resurrected_at: None,
            superseded_by: vec![],
            supersedes: None,
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
        task.after = vec!["nonexistent".to_string()];
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
        task.after = vec!["ghost".to_string()];
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

    #[test]
    fn test_show_verify_status_in_json() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Verify task");
        task.verify = Some("cargo test".to_string());
        task.verify_failures = 2;
        task.status = Status::InProgress;
        task.log.push(workgraph::graph::LogEntry {
            timestamp: "2026-01-01T00:00:00+00:00".to_string(),
            actor: Some("verify".to_string()),
            user: None,
            message: "Verify FAILED (exit code 1, attempt 2/3). Command: cargo test\nstderr: test failed".to_string(),
        });
        graph.add_node(Node::Task(task));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", true);
        assert!(result.is_ok());
    }

    #[test]
    fn test_show_verify_failures_display() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "Verify task");
        task.verify = Some("cargo test".to_string());
        task.verify_failures = 2;
        task.status = Status::InProgress;
        graph.add_node(Node::Task(task));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        // Should not panic and should succeed
        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_show_verify_circuit_breaker_tripped() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();

        let mut task = make_task("t1", "CB task");
        task.verify = Some("cargo test".to_string());
        task.verify_failures = 3;
        task.status = Status::Failed;
        task.failure_reason = Some("Circuit breaker tripped".to_string());
        task.log.push(workgraph::graph::LogEntry {
            timestamp: "2026-01-01T00:00:00+00:00".to_string(),
            actor: Some("verify-circuit-breaker".to_string()),
            user: None,
            message: "Circuit breaker tripped: verify command failed 3 times".to_string(),
        });
        graph.add_node(Node::Task(task));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }

    #[test]
    fn test_show_no_verify_section_when_no_verify() {
        // A task without verify should not display verify section
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().join("graph.jsonl");
        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(make_task("t1", "No verify task")));
        workgraph::parser::save_graph(&graph, &path).unwrap();

        let result = run(temp_dir.path(), "t1", false);
        assert!(result.is_ok());
    }
}
