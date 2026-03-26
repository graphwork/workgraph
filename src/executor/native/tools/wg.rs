//! In-process workgraph tools.
//!
//! These call workgraph library functions directly — no subprocess, no CLI parsing overhead.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;

use super::{Tool, ToolOutput, ToolRegistry, truncate_output};
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

        ToolOutput::success(truncate_output(output))
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
            ToolOutput::success(truncate_output(lines.join("\n")))
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
            description: "Create a new task in the workgraph.".to_string(),
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

        let (graph, path) = match load_workgraph(&self.dir) {
            Ok(g) => g,
            Err(e) => return ToolOutput::error(e),
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

        // Determine initial status
        let initial_status = if after.is_empty() {
            Status::Open
        } else {
            // Check if all dependencies are done
            let all_done = after.iter().all(|dep_id| {
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
            after: after.clone(),
            tags,
            skills,
            created_at: Some(Utc::now().to_rfc3339()),
            ..Default::default()
        };

        let task_id_clone = task_id.clone();
        let task_clone = task.clone();
        match modify_graph(&path, |graph| {
            graph.add_node(Node::Task(task_clone.clone()));
            true
        }) {
            Ok(_) => {}
            Err(e) => return ToolOutput::error(format!("Failed to save graph: {}", e)),
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
            return ToolOutput::error(msg);
        }

        ToolOutput::success(format!("Task '{}' marked as done", task_id))
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
