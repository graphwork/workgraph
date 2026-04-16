//! In-process workgraph tools.
//!
//! These call workgraph library functions directly — no subprocess, no CLI parsing overhead.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::{Tool, ToolOutput, ToolRegistry, truncate_for_tool};
use crate::executor::native::client::ToolDefinition;
use crate::graph::{LogEntry, Node, Status, Task};
use crate::parser::{load_graph, modify_graph};
use crate::query::build_reverse_index;

/// Register all workgraph tools.
pub fn register_wg_tools(registry: &mut ToolRegistry, workgraph_dir: PathBuf) {
    registry.register(Box::new(WgShowTool {
        dir: workgraph_dir.clone(),
    }));
    registry.register(Box::new(WgListTool {
        dir: workgraph_dir.clone(),
    }));
    registry.register(Box::new(WgAddTool {
        dir: workgraph_dir.clone(),
    }));
    registry.register(Box::new(WgDoneTool {
        dir: workgraph_dir.clone(),
    }));
    registry.register(Box::new(WgFailTool {
        dir: workgraph_dir.clone(),
    }));
    registry.register(Box::new(WgLogTool {
        dir: workgraph_dir.clone(),
    }));
    registry.register(Box::new(WgArtifactTool { dir: workgraph_dir }));
}

fn graph_path(dir: &Path) -> PathBuf {
    dir.join("graph.jsonl")
}

fn load_workgraph(dir: &Path) -> Result<(crate::graph::WorkGraph, PathBuf), String> {
    let path = graph_path(dir);
    if !path.exists() {
        return Err("Workgraph not initialized".to_string());
    }
    let graph = load_graph(&path).map_err(|e| format!("Failed to load graph: {}", e))?;
    Ok((graph, path))
}

fn default_parent_after(graph: &crate::graph::WorkGraph, after: &[String]) -> Vec<String> {
    if !after.is_empty() {
        return after.to_vec();
    }

    let Ok(current_task_id) = std::env::var("WG_TASK_ID") else {
        return vec![];
    };

    match graph.get_task(&current_task_id) {
        Some(task) if !task.tags.iter().any(|tag| tag == "coordinator-loop") => {
            vec![current_task_id]
        }
        _ => vec![],
    }
}

fn generate_id(title: &str) -> String {
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");

    // Truncate to reasonable length
    if slug.len() > 50 {
        slug[..slug.floor_char_boundary(50)].to_string()
    } else {
        slug
    }
}

// ── wg_show ─────────────────────────────────────────────────────────────

struct WgShowTool {
    dir: PathBuf,
}

#[async_trait]
impl Tool for WgShowTool {
    fn name(&self) -> &str {
        "wg_show"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wg_show".to_string(),
            description: "Show details of a workgraph task.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID to show"
                    }
                },
                "required": ["task_id"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let task_id = match input.get("task_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ToolOutput::error("Missing required parameter: task_id".to_string()),
        };

        let (graph, _path) = match load_workgraph(&self.dir) {
            Ok(g) => g,
            Err(e) => return ToolOutput::error(e),
        };

        let task = match graph.get_task(task_id) {
            Some(t) => t,
            None => return ToolOutput::error(format!("Task not found: {}", task_id)),
        };

        let reverse_index = build_reverse_index(&graph);
        let dependents: Vec<&str> = reverse_index
            .get(task_id)
            .map(|deps| deps.iter().map(|s| s.as_str()).collect())
            .unwrap_or_default();

        let mut output = format!(
            "Task: {}\nTitle: {}\nStatus: {}",
            task.id, task.title, task.status
        );

        if let Some(ref desc) = task.description {
            output.push_str(&format!("\nDescription: {}", desc));
        }
        if let Some(ref assigned) = task.assigned {
            output.push_str(&format!("\nAssigned: {}", assigned));
        }
        if !task.tags.is_empty() {
            output.push_str(&format!("\nTags: {}", task.tags.join(", ")));
        }
        if !task.after.is_empty() {
            output.push_str(&format!("\nAfter: {}", task.after.join(", ")));
        }
        if !dependents.is_empty() {
            output.push_str(&format!("\nBefore: {}", dependents.join(", ")));
        }
        if !task.artifacts.is_empty() {
            output.push_str(&format!("\nArtifacts: {}", task.artifacts.join(", ")));
        }
        if !task.log.is_empty() {
            output.push_str("\nLog:");
            for entry in &task.log {
                output.push_str(&format!("\n  {} {}", entry.timestamp, entry.message));
            }
        }

        ToolOutput::success(truncate_for_tool(&output, "wg_show"))
    }
}

// ── wg_list ─────────────────────────────────────────────────────────────

struct WgListTool {
    dir: PathBuf,
}

#[async_trait]
impl Tool for WgListTool {
    fn name(&self) -> &str {
        "wg_list"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wg_list".to_string(),
            description: "List tasks in the workgraph, optionally filtered by status.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "Filter by status: open, in-progress, done, blocked, failed"
                    }
                }
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let status_filter = input.get("status").and_then(|v| v.as_str());

        let (graph, _path) = match load_workgraph(&self.dir) {
            Ok(g) => g,
            Err(e) => return ToolOutput::error(e),
        };

        let target_status: Option<Status> = match status_filter {
            Some("open") => Some(Status::Open),
            Some("in-progress") => Some(Status::InProgress),
            Some("done") => Some(Status::Done),
            Some("blocked") => Some(Status::Blocked),
            Some("failed") => Some(Status::Failed),
            Some("abandoned") => Some(Status::Abandoned),
            Some(other) => {
                return ToolOutput::error(format!("Unknown status filter: {}", other));
            }
            None => None,
        };

        let mut lines = Vec::new();
        for task in graph.tasks() {
            if let Some(ref target) = target_status
                && task.status != *target
            {
                continue;
            }
            lines.push(format!("{}\t{}\t{}", task.id, task.status, task.title));
        }

        if lines.is_empty() {
            ToolOutput::success("No tasks found.".to_string())
        } else {
            ToolOutput::success(truncate_for_tool(&lines.join("\n"), "wg_list"))
        }
    }
}

// ── wg_add ──────────────────────────────────────────────────────────────

struct WgAddTool {
    dir: PathBuf,
}

#[async_trait]
impl Tool for WgAddTool {
    fn name(&self) -> &str {
        "wg_add"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wg_add".to_string(),
            description: "Create a new task in the workgraph. Supports subtask delegation \
                (block current task until child completes) and cron scheduling."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "Title for the new task"
                    },
                    "description": {
                        "type": "string",
                        "description": "Detailed description of the task"
                    },
                    "after": {
                        "type": "string",
                        "description": "Comma-separated dependency task IDs"
                    },
                    "tags": {
                        "type": "string",
                        "description": "Comma-separated tags"
                    },
                    "skills": {
                        "type": "string",
                        "description": "Comma-separated skills"
                    },
                    "subtask": {
                        "type": "boolean",
                        "description": "If true, treat as a blocking subtask of the current task. The current task is parked (Waiting) until this child completes. Requires WG_TASK_ID to be set in the agent environment."
                    },
                    "cron": {
                        "type": "string",
                        "description": "5-field cron expression (e.g. '*/5 * * * *' for every 5 minutes, '0 2 * * *' for daily at 2am). Creates a calendar-scheduled task that fires periodically instead of on dependency completion."
                    }
                },
                "required": ["title"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let title = match input.get("title").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => t,
            _ => {
                return ToolOutput::error("Missing or empty required parameter: title".to_string());
            }
        };

        let description = input.get("description").and_then(|v| v.as_str());
        let after_str = input.get("after").and_then(|v| v.as_str()).unwrap_or("");
        let tags_str = input.get("tags").and_then(|v| v.as_str()).unwrap_or("");
        let skills_str = input.get("skills").and_then(|v| v.as_str()).unwrap_or("");
        let is_subtask = input
            .get("subtask")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let cron_expr = input.get("cron").and_then(|v| v.as_str());

        let after: Vec<String> = after_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let tags: Vec<String> = tags_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let skills: Vec<String> = skills_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // --subtask: requires WG_TASK_ID to identify the parent task.
        let subtask_parent_id: Option<String> = if is_subtask {
            match std::env::var("WG_TASK_ID") {
                Ok(pid) if !pid.is_empty() => Some(pid),
                _ => {
                    return ToolOutput::error(
                        "subtask=true requires WG_TASK_ID to be set in the agent environment \
                         (the in-process wg_add tool uses it to identify the parent task). \
                         This should be set automatically when spawned by the coordinator."
                            .to_string(),
                    );
                }
            }
        } else {
            None
        };

        // --cron: parse expression and compute next fire time up-front so
        // invalid cron rejects the task creation cleanly.
        let (cron_schedule, cron_enabled, next_cron_fire) = if let Some(expr) = cron_expr {
            match crate::cron::parse_cron_expression(expr) {
                Ok(schedule) => {
                    let next_fire = crate::cron::calculate_next_fire(&schedule, Utc::now());
                    (
                        Some(expr.to_string()),
                        true,
                        next_fire.map(|dt| dt.to_rfc3339()),
                    )
                }
                Err(e) => {
                    return ToolOutput::error(format!("Invalid cron expression '{}': {}", expr, e));
                }
            }
        } else {
            (None, false, None)
        };

        let (graph, path) = match load_workgraph(&self.dir) {
            Ok(g) => g,
            Err(e) => return ToolOutput::error(e),
        };
        // For subtask, we bypass the default-parent injection because the
        // parent handles the blocking relationship via wait_condition, not
        // via the normal after-dependency mechanism.
        let effective_after = if is_subtask {
            after.clone()
        } else {
            default_parent_after(&graph, &after)
        };

        // Generate a unique task ID from title
        let mut task_id = generate_id(title);
        // Ensure uniqueness
        let mut counter = 2;
        let base_id = task_id.clone();
        while graph.get_node(&task_id).is_some() {
            task_id = format!("{}-{}", base_id, counter);
            counter += 1;
        }

        // Determine initial status. Subtasks always start Open so the
        // coordinator can immediately dispatch them — the parent's
        // wait_condition is what enforces blocking.
        let initial_status = if is_subtask || effective_after.is_empty() {
            Status::Open
        } else {
            // Check if all dependencies are done
            let all_done = effective_after.iter().all(|dep_id| {
                graph
                    .get_task(dep_id)
                    .map(|t| t.status == Status::Done)
                    .unwrap_or(false)
            });
            if all_done {
                Status::Open
            } else {
                Status::Blocked
            }
        };

        let task = Task {
            id: task_id.clone(),
            title: title.to_string(),
            description: description.map(|s| s.to_string()),
            status: initial_status,
            after: effective_after.clone(),
            tags,
            skills,
            created_at: Some(Utc::now().to_rfc3339()),
            cron_schedule,
            cron_enabled,
            next_cron_fire,
            unplaced: is_subtask,
            ..Default::default()
        };

        let task_id_clone = task_id.clone();
        let task_clone = task.clone();
        match modify_graph(&path, |graph| {
            graph.add_node(Node::Task(task_clone.clone()));
            for dep in &task_clone.after {
                if let Some(blocker) = graph.get_task_mut(dep)
                    && !blocker.before.contains(&task_id_clone)
                {
                    blocker.before.push(task_id_clone.clone());
                }
            }
            true
        }) {
            Ok(_) => {}
            Err(e) => return ToolOutput::error(format!("Failed to save graph: {}", e)),
        }

        // --subtask: park the parent with a wait_condition for this child.
        if let Some(ref parent_id) = subtask_parent_id {
            let child_id = task_id.clone();
            let parent_id_s = parent_id.clone();
            let mut wait_error: Option<String> = None;

            match modify_graph(&path, |graph| {
                let parent = match graph.get_task(&parent_id_s) {
                    Some(t) => t,
                    None => {
                        wait_error = Some(format!(
                            "Parent task '{}' not found (WG_TASK_ID is stale?)",
                            parent_id_s
                        ));
                        return false;
                    }
                };
                if parent.status != Status::InProgress {
                    wait_error = Some(format!(
                        "Cannot set subtask wait on parent '{}': status is '{}', expected 'in-progress'",
                        parent_id_s, parent.status
                    ));
                    return false;
                }
                let parent = graph.get_task_mut(&parent_id_s).expect("verified above");
                parent.status = Status::Waiting;
                parent.wait_condition = Some(crate::graph::WaitSpec::Any(vec![
                    crate::graph::WaitCondition::TaskStatus {
                        task_id: child_id.clone(),
                        status: Status::Done,
                    },
                    crate::graph::WaitCondition::TaskStatus {
                        task_id: child_id.clone(),
                        status: Status::Failed,
                    },
                ]));
                parent.log.push(crate::graph::LogEntry {
                    timestamp: Utc::now().to_rfc3339(),
                    actor: parent.assigned.clone(),
                    user: Some(crate::current_user()),
                    message: format!(
                        "Agent parked. Waiting for subtask '{}' to complete.",
                        child_id
                    ),
                });
                true
            }) {
                Ok(_) => {}
                Err(e) => {
                    return ToolOutput::error(format!(
                        "Created task '{}' but failed to park parent: {}",
                        task_id_clone, e
                    ));
                }
            }
            if let Some(msg) = wait_error {
                return ToolOutput::error(format!(
                    "Created task '{}' but failed to park parent: {}",
                    task_id_clone, msg
                ));
            }

            return ToolOutput::success(format!(
                "Created subtask: {} (parent '{}' parked until child completes)",
                task_id_clone, parent_id
            ));
        }

        // --cron: mention the schedule in the success message so the agent
        // can verify it was accepted.
        if let Some(expr) = cron_expr {
            return ToolOutput::success(format!(
                "Created cron task: {} (schedule: '{}')",
                task_id_clone, expr
            ));
        }

        ToolOutput::success(format!("Created task: {}", task_id_clone))
    }
}

// ── wg_done ─────────────────────────────────────────────────────────────

struct WgDoneTool {
    dir: PathBuf,
}

#[async_trait]
impl Tool for WgDoneTool {
    fn name(&self) -> &str {
        "wg_done"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wg_done".to_string(),
            description: "Mark a task as done.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID to mark as done"
                    },
                    "converged": {
                        "type": "boolean",
                        "description": "Use for cycle convergence (stops further iterations)"
                    }
                },
                "required": ["task_id"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let task_id = match input.get("task_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ToolOutput::error("Missing required parameter: task_id".to_string()),
        };

        let converged = input
            .get("converged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let path = graph_path(&self.dir);
        if !path.exists() {
            return ToolOutput::error("Workgraph not initialized".to_string());
        }

        let mut result_msg: Option<String> = None;
        match modify_graph(&path, |graph| {
            let task = match graph.get_task_mut(task_id) {
                Some(t) => t,
                None => {
                    result_msg = Some(format!("Task not found: {}", task_id));
                    return false;
                }
            };

            // Idempotent: if already done, don't re-mark. This
            // prevents agents from looping on wg_done calls.
            if task.status == Status::Done {
                result_msg = Some(format!(
                    "Task '{}' is already done. You have completed your work — \
                     stop here, do NOT call wg_done again.",
                    task_id
                ));
                return false;
            }

            task.status = Status::Done;
            task.completed_at = Some(Utc::now().to_rfc3339());

            if converged && !task.tags.contains(&"converged".to_string()) {
                task.tags.push("converged".to_string());
            }
            true
        }) {
            Ok(_) => {}
            Err(e) => return ToolOutput::error(format!("Failed to save graph: {}", e)),
        }
        if let Some(msg) = result_msg {
            // "already done" is a success, not an error — the task
            // IS done, which is what the agent wanted. Returning it
            // as success prevents the agent from retrying.
            if msg.contains("already done") {
                return ToolOutput::success(msg);
            }
            return ToolOutput::error(msg);
        }

        ToolOutput::success(format!(
            "Task '{}' marked as done. Your work is complete — stop here.",
            task_id
        ))
    }
}

// ── wg_fail ─────────────────────────────────────────────────────────────

struct WgFailTool {
    dir: PathBuf,
}

#[async_trait]
impl Tool for WgFailTool {
    fn name(&self) -> &str {
        "wg_fail"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wg_fail".to_string(),
            description: "Mark a task as failed.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID to mark as failed"
                    },
                    "reason": {
                        "type": "string",
                        "description": "Reason for failure"
                    }
                },
                "required": ["task_id"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let task_id = match input.get("task_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ToolOutput::error("Missing required parameter: task_id".to_string()),
        };

        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("No reason provided");

        let path = graph_path(&self.dir);
        if !path.exists() {
            return ToolOutput::error("Workgraph not initialized".to_string());
        }

        let reason_owned = reason.to_string();
        let mut result_msg: Option<String> = None;
        match modify_graph(&path, |graph| {
            let task = match graph.get_task_mut(task_id) {
                Some(t) => t,
                None => {
                    result_msg = Some(format!("Task not found: {}", task_id));
                    return false;
                }
            };

            task.status = Status::Failed;
            task.completed_at = Some(Utc::now().to_rfc3339());

            // Log the failure reason
            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("native-agent".to_string()),
                user: Some(crate::current_user()),
                message: format!("Failed: {}", reason_owned),
            });
            true
        }) {
            Ok(_) => {}
            Err(e) => return ToolOutput::error(format!("Failed to save graph: {}", e)),
        }
        if let Some(msg) = result_msg {
            return ToolOutput::error(msg);
        }

        ToolOutput::success(format!("Task '{}' marked as failed: {}", task_id, reason))
    }
}

// ── wg_log ──────────────────────────────────────────────────────────────

struct WgLogTool {
    dir: PathBuf,
}

#[async_trait]
impl Tool for WgLogTool {
    fn name(&self) -> &str {
        "wg_log"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wg_log".to_string(),
            description: "Append a log entry to a task.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID to log to"
                    },
                    "message": {
                        "type": "string",
                        "description": "Log message"
                    }
                },
                "required": ["task_id", "message"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let task_id = match input.get("task_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ToolOutput::error("Missing required parameter: task_id".to_string()),
        };
        let message = match input.get("message").and_then(|v| v.as_str()) {
            Some(m) => m,
            None => return ToolOutput::error("Missing required parameter: message".to_string()),
        };

        let path = graph_path(&self.dir);
        if !path.exists() {
            return ToolOutput::error("Workgraph not initialized".to_string());
        }

        let message_owned = message.to_string();
        let mut result_msg: Option<String> = None;
        match modify_graph(&path, |graph| {
            let task = match graph.get_task_mut(task_id) {
                Some(t) => t,
                None => {
                    result_msg = Some(format!("Task not found: {}", task_id));
                    return false;
                }
            };

            task.log.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                actor: Some("native-agent".to_string()),
                user: Some(crate::current_user()),
                message: message_owned.clone(),
            });
            true
        }) {
            Ok(_) => {}
            Err(e) => return ToolOutput::error(format!("Failed to save graph: {}", e)),
        }
        if let Some(msg) = result_msg {
            return ToolOutput::error(msg);
        }

        ToolOutput::success(format!("Added log entry to '{}'", task_id))
    }
}

// ── wg_artifact ─────────────────────────────────────────────────────────

struct WgArtifactTool {
    dir: PathBuf,
}

#[async_trait]
impl Tool for WgArtifactTool {
    fn name(&self) -> &str {
        "wg_artifact"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "wg_artifact".to_string(),
            description: "Record an artifact (file path) for a task.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "The task ID to record the artifact for"
                    },
                    "path": {
                        "type": "string",
                        "description": "Path to the artifact file"
                    }
                },
                "required": ["task_id", "path"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let task_id = match input.get("task_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ToolOutput::error("Missing required parameter: task_id".to_string()),
        };
        let path = match input.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolOutput::error("Missing required parameter: path".to_string()),
        };

        let gpath = graph_path(&self.dir);
        if !gpath.exists() {
            return ToolOutput::error("Workgraph not initialized".to_string());
        }

        let path_owned = path.to_string();
        let mut result_msg: Option<String> = None;
        match modify_graph(&gpath, |graph| {
            let task = match graph.get_task_mut(task_id) {
                Some(t) => t,
                None => {
                    result_msg = Some(format!("Task not found: {}", task_id));
                    return false;
                }
            };

            if !task.artifacts.contains(&path_owned) {
                task.artifacts.push(path_owned.clone());
            }
            true
        }) {
            Ok(_) => {}
            Err(e) => return ToolOutput::error(format!("Failed to save graph: {}", e)),
        }
        if let Some(msg) = result_msg {
            return ToolOutput::error(msg);
        }

        ToolOutput::success(format!(
            "Recorded artifact '{}' for task '{}'",
            path, task_id
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    use crate::graph::{Node, Task, WorkGraph};
    use crate::parser::{load_graph, save_graph};
    use serde_json::json;

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn stub_task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            ..Task::default()
        }
    }

    #[tokio::test]
    async fn wg_add_defaults_after_to_current_task() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let graph_path = dir.path().join("graph.jsonl");

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(stub_task("parent-task")));
        save_graph(&graph, &graph_path).unwrap();

        unsafe { std::env::set_var("WG_TASK_ID", "parent-task") };

        let tool = WgAddTool {
            dir: dir.path().to_path_buf(),
        };
        let result = tool.execute(&json!({ "title": "Child task" })).await;
        unsafe { std::env::remove_var("WG_TASK_ID") };

        assert!(!result.is_error, "{}", result.content);

        let graph = load_graph(&graph_path).unwrap();
        let child = graph.get_task("child-task").unwrap();
        assert_eq!(child.after, vec!["parent-task".to_string()]);

        let parent = graph.get_task("parent-task").unwrap();
        assert!(parent.before.contains(&"child-task".to_string()));
    }

    #[tokio::test]
    async fn wg_add_skips_default_after_for_coordinator_task() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let graph_path = dir.path().join("graph.jsonl");

        let mut coordinator = stub_task("coordinator-task");
        coordinator.tags.push("coordinator-loop".to_string());

        let mut graph = WorkGraph::new();
        graph.add_node(Node::Task(coordinator));
        save_graph(&graph, &graph_path).unwrap();

        unsafe { std::env::set_var("WG_TASK_ID", "coordinator-task") };

        let tool = WgAddTool {
            dir: dir.path().to_path_buf(),
        };
        let result = tool.execute(&json!({ "title": "Orphan task" })).await;
        unsafe { std::env::remove_var("WG_TASK_ID") };

        assert!(!result.is_error, "{}", result.content);

        let graph = load_graph(&graph_path).unwrap();
        let orphan = graph.get_task("orphan-task").unwrap();
        assert!(orphan.after.is_empty());
    }
}
