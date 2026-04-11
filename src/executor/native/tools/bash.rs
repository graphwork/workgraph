//! Bash tool: execute shell commands with timeout and streaming output.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::{Tool, ToolOutput, ToolStreamCallback, truncate_for_tool};
use crate::executor::native::client::ToolDefinition;

const DEFAULT_TIMEOUT_MS: u64 = 300_000; // 5 minutes
const MAX_TIMEOUT_MS: u64 = 900_000; // 15 minutes

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
            description:
                "Execute a shell command and return its output (stdout + stderr). For long-running \
                 commands (cargo build, cargo test, etc.), specify a higher timeout."
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
                        "description": "Timeout in milliseconds (default: 300000, max: 900000). Examples: no timeout needed for quick commands (default applies), cargo build/test: 300000-600000, very long operations: 600000+."
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
                        ToolOutput::success(truncate_for_tool(&result, "bash"))
                    }
                } else {
                    let code = output.status.code().unwrap_or(-1);
                    if result.is_empty() {
                        ToolOutput::error(format!("Command exited with code {}", code))
                    } else {
                        ToolOutput::error(truncate_for_tool(
                            &format!("Exit code: {}\n{}", code, result),
                            "bash",
                        ))
                    }
                }
            }
            Ok(Err(e)) => ToolOutput::error(format!("Failed to execute command: {}", e)),
            Err(_) => ToolOutput::error(format!("Command timed out after {}ms", timeout_ms)),
        }
    }

    async fn execute_streaming(
        &self,
        input: &serde_json::Value,
        on_chunk: ToolStreamCallback,
    ) -> ToolOutput {
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

        let mut child = match tokio::time::timeout(timeout, async {
            Command::new("bash")
                .arg("-c")
                .arg(command)
                .current_dir(&self.working_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        })
        .await
        {
            Ok(Ok(child)) => child,
            Ok(Err(e)) => return ToolOutput::error(format!("Failed to spawn command: {}", e)),
            Err(_) => {
                return ToolOutput::error(format!("Command timed out after {}ms", timeout_ms));
            }
        };

        let stdout = child.stdout.take().expect("stdout pipe");
        let stderr = child.stderr.take().expect("stderr pipe");

        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        let mut accumulated = String::new();

        loop {
            tokio::select! {
                line = stdout_reader.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            accumulated.push_str(&line);
                            accumulated.push('\n');
                            on_chunk(line);
                        }
                        Ok(None) => {}
                        Err(_) => {}
                    }
                }
                err_line = stderr_reader.next_line() => {
                    match err_line {
                        Ok(Some(line)) => {
                            accumulated.push_str("[stderr]\n");
                            accumulated.push_str(&line);
                            accumulated.push('\n');
                            on_chunk(format!("[stderr] {}", line));
                        }
                        Ok(None) => {}
                        Err(_) => {}
                    }
                }
                status = child.wait() => {
                    match status {
                        Ok(exit_status) => {
                            let result = if exit_status.success() {
                                if accumulated.is_empty() {
                                    ToolOutput::success("(no output)".to_string())
                                } else {
                                    ToolOutput::success(truncate_for_tool(&accumulated, "bash"))
                                }
                            } else {
                                let code = exit_status.code().unwrap_or(-1);
                                if accumulated.is_empty() {
                                    ToolOutput::error(format!("Command exited with code {}", code))
                                } else {
                                    ToolOutput::error(truncate_for_tool(
                                        &format!("Exit code: {}\n{}", code, accumulated),
                                        "bash",
                                    ))
                                }
                            };
                            return result;
                        }
                        Err(e) => {
                            return ToolOutput::error(format!("Failed to wait on child: {}", e));
                        }
                    }
                }
            }
        }
    }
}
