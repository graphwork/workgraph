//! Background task tool: manage detached background jobs.
//!
//! Provides the `bg` tool for launching and managing background tasks
//! that run detached from the turn loop.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Mutex as TokioMutex;

use super::{Tool, ToolOutput};
use crate::executor::native::background::{Job, JobStore};
use crate::executor::native::client::ToolDefinition;

/// The bg tool for managing background tasks.
pub struct BackgroundTool {
    /// Path to the workgraph directory.
    workgraph_dir: PathBuf,
    /// Shared job store with async mutex.
    job_store: Arc<TokioMutex<Option<JobStore>>>,
}

impl BackgroundTool {
    /// Create a new BackgroundTool.
    pub fn new(workgraph_dir: PathBuf) -> Self {
        Self {
            workgraph_dir,
            job_store: Arc::new(TokioMutex::new(None)),
        }
    }

    /// Initialize the job store if not already initialized.
    async fn ensure_store(&self) -> Result<(), String> {
        let mut store: tokio::sync::MutexGuard<'_, Option<JobStore>> = self.job_store.lock().await;
        if store.is_none() {
            match JobStore::new(self.workgraph_dir.clone()) {
                Ok(s) => *store = Some(s),
                Err(e) => return Err(format!("Failed to initialize job store: {}", e)),
            }
        }
        Ok(())
    }

    /// Format a job for JSON output.
    fn format_job(&self, job: &Job) -> serde_json::Value {
        let output_lines = if job.log_path.exists() {
            std::fs::read_to_string(&job.log_path)
                .map(|s| s.lines().count())
                .unwrap_or(0)
        } else {
            0
        };

        json!({
            "id": job.id,
            "name": job.name,
            "command": job.command,
            "status": format!("{:?}", job.status).to_lowercase(),
            "pid": job.pid,
            "created_at": job.created_at.to_rfc3339(),
            "output_lines": output_lines,
            "exit_code": job.exit_code,
        })
    }
}

#[async_trait]
impl Tool for BackgroundTool {
    fn name(&self) -> &str {
        "bg"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bg".to_string(),
            description: "Manage background tasks that run detached from the turn loop. \
                Use this to run long-running commands (cargo build, cargo test, servers) \
                without blocking the agent. Jobs persist across agent restarts."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["run", "status", "kill", "list", "output", "delete"],
                        "description": "The action to perform"
                    },
                    "command": {
                        "type": "string",
                        "description": "For run: the command to execute"
                    },
                    "job": {
                        "type": "string",
                        "description": "For status/kill/output/delete: job identifier (ID or name)"
                    },
                    "name": {
                        "type": "string",
                        "description": "Optional friendly name for the job (for run action)"
                    },
                    "lines": {
                        "type": "integer",
                        "description": "For output: number of lines to return (default: all)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let action = match input.get("action").and_then(|v| v.as_str()) {
            Some(a) => a,
            None => {
                return ToolOutput::error("Missing required parameter: action".to_string());
            }
        };

        // Initialize job store if needed
        if let Err(e) = self.ensure_store().await {
            return ToolOutput::error(e);
        }

        // Get exclusive access to job store for the action
        let mut store_guard: tokio::sync::MutexGuard<'_, Option<JobStore>> =
            self.job_store.lock().await;
        let store = match store_guard.as_mut() {
            Some(s) => s,
            None => return ToolOutput::error("Job store not initialized".to_string()),
        };

        match action {
            "run" => self.run_action(input, store).await,
            "status" => self.status_action(input, store),
            "list" => self.list_action(store),
            "kill" => self.kill_action(input, store).await,
            "output" => self.output_action(input, store),
            "delete" => self.delete_action(input, store).await,
            other => ToolOutput::error(format!(
                "Unknown action: {}. Valid actions: run, status, list, kill, output, delete",
                other
            )),
        }
    }

    fn is_read_only(&self) -> bool {
        // Most actions modify state, so we treat as non-read-only
        false
    }
}

impl BackgroundTool {
    /// Handle the 'run' action.
    async fn run_action(&self, input: &serde_json::Value, store: &mut JobStore) -> ToolOutput {
        let command = match input.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => {
                return ToolOutput::error("Missing required parameter: command".to_string());
            }
        };

        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let cmd_short = command
                    .split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("_");
                format!("job-{}", cmd_short)
            });

        let working_dir = input
            .get("working_dir")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        match store.run(&name, command, &working_dir).await {
            Ok(job) => {
                let output = json!({
                    "jobs": [self.format_job(&job)]
                });
                ToolOutput::success(output.to_string())
            }
            Err(e) => ToolOutput::error(format!("Failed to run job: {}", e)),
        }
    }

    /// Handle the 'status' action.
    fn status_action(&self, input: &serde_json::Value, store: &JobStore) -> ToolOutput {
        let job_id = match input.get("job").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return ToolOutput::error("Missing required parameter: job".to_string());
            }
        };

        match store.get(job_id) {
            Some(job) => {
                let output = json!({
                    "jobs": [self.format_job(job)]
                });
                ToolOutput::success(output.to_string())
            }
            None => ToolOutput::error(format!("Job not found: {}", job_id)),
        }
    }

    /// Handle the 'list' action.
    fn list_action(&self, store: &JobStore) -> ToolOutput {
        let jobs: Vec<serde_json::Value> =
            store.list().iter().map(|j| self.format_job(j)).collect();

        let output = json!({ "jobs": jobs });
        ToolOutput::success(output.to_string())
    }

    /// Handle the 'kill' action.
    async fn kill_action(&self, input: &serde_json::Value, store: &mut JobStore) -> ToolOutput {
        let job_id = match input.get("job").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return ToolOutput::error("Missing required parameter: job".to_string());
            }
        };

        match store.kill(job_id).await {
            Ok(()) => {
                let job = store.get(job_id).unwrap();
                let output = json!({
                    "jobs": [self.format_job(job)]
                });
                ToolOutput::success(output.to_string())
            }
            Err(e) => ToolOutput::error(format!("Failed to kill job: {}", e)),
        }
    }

    /// Handle the 'output' action.
    fn output_action(&self, input: &serde_json::Value, store: &JobStore) -> ToolOutput {
        let job_id = match input.get("job").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return ToolOutput::error("Missing required parameter: job".to_string());
            }
        };

        let lines = input
            .get("lines")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize);

        match store.output(job_id, lines) {
            Ok(output_str) => {
                let truncated = truncate_bg_output(&output_str);
                ToolOutput::success(truncated)
            }
            Err(e) => ToolOutput::error(format!("Failed to get output: {}", e)),
        }
    }

    /// Handle the 'delete' action.
    async fn delete_action(&self, input: &serde_json::Value, store: &mut JobStore) -> ToolOutput {
        let job_id = match input.get("job").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => {
                return ToolOutput::error("Missing required parameter: job".to_string());
            }
        };

        match store.delete(job_id).await {
            Ok(()) => ToolOutput::success(
                json!({
                    "deleted": job_id
                })
                .to_string(),
            ),
            Err(e) => ToolOutput::error(format!("Failed to delete job: {}", e)),
        }
    }
}

/// Truncate background job output to a reasonable size.
fn truncate_bg_output(output: &str) -> String {
    const MAX_CHARS: usize = 8000;

    if output.len() <= MAX_CHARS {
        return output.to_string();
    }

    let head_end = output.floor_char_boundary(MAX_CHARS / 2);
    let tail_start = output.floor_char_boundary(output.len() - MAX_CHARS / 2);

    format!(
        "{}...\n\n[{} chars omitted]\n\n...{}",
        &output[..head_end],
        output.len() - head_end - (output.len() - tail_start),
        &output[tail_start..]
    )
}

/// Register the bg tool.
pub fn register_bg_tool(registry: &mut super::ToolRegistry, workgraph_dir: PathBuf) {
    registry.register(Box::new(BackgroundTool::new(workgraph_dir)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_bg_tool_list() {
        let tmp = TempDir::new().unwrap();
        let tool = BackgroundTool::new(tmp.path().to_path_buf());

        let input = json!({ "action": "list" });
        let result = tool.execute(&input).await;

        assert!(!result.is_error);
        assert!(result.content.contains("\"jobs\":[]"));
    }

    #[tokio::test]
    async fn test_bg_tool_run() {
        let tmp = TempDir::new().unwrap();
        let tool = BackgroundTool::new(tmp.path().to_path_buf());

        let input = json!({
            "action": "run",
            "command": "echo hello world",
            "name": "test-echo"
        });
        let result = tool.execute(&input).await;

        assert!(!result.is_error, "Error: {}", result.content);
        assert!(result.content.contains("test-echo"));
        assert!(result.content.contains("\"status\":\"running\""));

        // Wait for completion and cleanup
        tokio::time::sleep(Duration::from_millis(200)).await;
        let delete_input = json!({ "action": "delete", "job": "test-echo" });
        let _ = tool.execute(&delete_input).await;
    }

    #[tokio::test]
    async fn test_bg_tool_kill() {
        let tmp = TempDir::new().unwrap();
        let tool = BackgroundTool::new(tmp.path().to_path_buf());

        // Run a long job
        let run_input = json!({
            "action": "run",
            "command": "sleep 60",
            "name": "kill-test"
        });
        tool.execute(&run_input).await;

        // Kill it
        let kill_input = json!({ "action": "kill", "job": "kill-test" });
        let result = tool.execute(&kill_input).await;

        assert!(!result.is_error, "Error: {}", result.content);
        assert!(result.content.contains("\"status\":\"cancelled\""));

        // Cleanup
        let delete_input = json!({ "action": "delete", "job": "kill-test" });
        let _ = tool.execute(&delete_input).await;
    }

    #[tokio::test]
    async fn test_bg_tool_output() {
        let tmp = TempDir::new().unwrap();
        let tool = BackgroundTool::new(tmp.path().to_path_buf());

        // Run a job that produces output
        let run_input = json!({
            "action": "run",
            "command": "echo hello world",
            "name": "output-test"
        });
        tool.execute(&run_input).await;

        // Small delay to let output flush
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Get output
        let output_input = json!({ "action": "output", "job": "output-test" });
        let result = tool.execute(&output_input).await;

        assert!(!result.is_error, "Error: {}", result.content);

        // Cleanup
        let delete_input = json!({ "action": "delete", "job": "output-test" });
        let _ = tool.execute(&delete_input).await;
    }

    #[tokio::test]
    async fn test_bg_tool_missing_action() {
        let tmp = TempDir::new().unwrap();
        let tool = BackgroundTool::new(tmp.path().to_path_buf());

        let input = json!({});
        let result = tool.execute(&input).await;

        assert!(result.is_error);
        assert!(
            result
                .content
                .contains("Missing required parameter: action")
        );
    }

    #[tokio::test]
    async fn test_bg_tool_missing_job() {
        let tmp = TempDir::new().unwrap();
        let tool = BackgroundTool::new(tmp.path().to_path_buf());

        let input = json!({ "action": "status" });
        let result = tool.execute(&input).await;

        assert!(result.is_error);
        assert!(result.content.contains("Missing required parameter: job"));
    }
}
