//! Bash tool: execute shell commands with timeout.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tokio::process::Command;

use super::{Tool, ToolOutput, truncate_output};
use crate::executor::native::client::ToolDefinition;

const DEFAULT_TIMEOUT_MS: u64 = 120_000; // 2 minutes
const MAX_TIMEOUT_MS: u64 = 600_000; // 10 minutes

/// Register the bash tool.
pub fn register_bash_tool(registry: &mut super::ToolRegistry, working_dir: PathBuf) {
    registry.register(Box::new(BashTool { working_dir }));
}

struct BashTool {
    working_dir: PathBuf,
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "bash".to_string(),
            description: "Execute a shell command and return its output (stdout + stderr)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute"
                    },
                    "timeout": {
                        "type": "integer",
                        "description": "Timeout in milliseconds (default: 120000, max: 600000)"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, input: &serde_json::Value) -> ToolOutput {
        let command = match input.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return ToolOutput::error("Missing required parameter: command".to_string()),
        };

        let timeout_ms = input
            .get("timeout")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        let timeout = Duration::from_millis(timeout_ms);

        let result = tokio::time::timeout(timeout, async {
            Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(&self.working_dir)
                .output()
                .await
        })
        .await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                let mut result = String::new();
                if !stdout.is_empty() {
                    result.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str("[stderr]\n");
                    result.push_str(&stderr);
                }

                if output.status.success() {
                    if result.is_empty() {
                        ToolOutput::success("(no output)".to_string())
                    } else {
                        ToolOutput::success(truncate_output(result))
                    }
                } else {
                    let code = output.status.code().unwrap_or(-1);
                    if result.is_empty() {
                        ToolOutput::error(format!("Command exited with code {}", code))
                    } else {
                        ToolOutput::error(truncate_output(format!(
                            "Exit code: {}\n{}",
                            code, result
                        )))
                    }
                }
            }
            Ok(Err(e)) => ToolOutput::error(format!("Failed to execute command: {}", e)),
            Err(_) => ToolOutput::error(format!("Command timed out after {}ms", timeout_ms)),
        }
    }
}
